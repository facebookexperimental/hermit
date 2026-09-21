/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls for dealing with the file system.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;

use nix::fcntl::AtFlags;
use nix::fcntl::OFlag;
use rand::RngExt as _;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::FcntlCmd::*;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::SockFlag;
use reverie::syscalls::StatPtr;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie::syscalls::Timespec;
use reverie::syscalls::Whence;
use reverie::syscalls::family::StatFamily;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;

use super::deterministic_stdio_inode_for_resource;
use crate::config::SchedHeuristic;
use crate::dirents::*;
use crate::fd::*;
use crate::procfs::MountInfoSnapshot;
use crate::procfs::ProcfsFile;
use crate::procfs::ProcfsSnapshotContext;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Device;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::resources::SABRE_INTERNAL_PIPE_IO_FYI;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::stat::*;
use crate::tool_global::*;
use crate::tool_local::CapturedDetFdInstallError;
use crate::tool_local::Detcore;
use crate::tool_local::finish_partial_record_or_replay_write;
use crate::types::*;

/// A conversion from SOCK_* flags to O_* flags which makes unsafe (but checked during testing) assumptions.
fn oflag_from_sock_bits(s_bits: i32) -> OFlag {
    // An otherwise unsafe "cast" which leans on the `linux_flags_assumptions` below.
    OFlag::from_bits_truncate(s_bits & (libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK))
}

const UNIX_AUTOBIND_NAME_LEN: usize = 6;
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-2150): Review timer-slack procfs parsing,
// per-operation target checks, and scalar/vector I/O emulation.
const TIMER_SLACK_PARSE_BYTES: usize = 66;
#[derive(Clone, Copy)]
struct TimerSlackIovec {
    base: usize,
    len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TimerSlackBinding {
    target: i32,
    device: u64,
    inode: u64,
}

fn classify_timer_slack_binding(
    binding: TimerSlackBinding,
    observed_identity: Option<(u64, u64)>,
    current_tid: i32,
) -> Result<(), Errno> {
    if observed_identity != Some((binding.device, binding.inode)) {
        // The bound task exited. A missing path and a new task that recycled
        // the same numeric TID are both ESRCH for the old open inode.
        return Err(Errno::ESRCH);
    }
    if current_tid != binding.target {
        // Cross-task CAP_SYS_NICE access is intentionally not exposed.
        return Err(Errno::EPERM);
    }
    Ok(())
}

fn parse_timer_slack_write(bytes: &[u8]) -> Result<u64, Errno> {
    // `kstrtoull_from_user` copies at most sign + 64 binary digits + newline,
    // then accepts decimal digits with one optional leading '+' and one
    // optional trailing newline. An embedded NUL terminates the C string.
    let bytes = &bytes[..bytes.len().min(TIMER_SLACK_PARSE_BYTES)];
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let mut value = &bytes[..end];
    if value.first() == Some(&b'+') {
        value = &value[1..];
    }
    if value.last() == Some(&b'\n') {
        value = &value[..value.len() - 1];
    }
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(Errno::EINVAL);
    }
    value.iter().try_fold(0_u64, |parsed, digit| {
        parsed
            .checked_mul(10)
            .and_then(|parsed| parsed.checked_add(u64::from(digit - b'0')))
            .ok_or(Errno::ERANGE)
    })
}

fn vectored_offset(low: u64, high: u64) -> i64 {
    if std::mem::size_of::<usize>() == 8 {
        low as i64
    } else {
        ((high << 32) | (low & u32::MAX as u64)) as i64
    }
}

fn read_iovecs<M: MemoryAccess>(
    memory: &M,
    address: Option<Addr<libc::iovec>>,
    count: usize,
) -> Result<Vec<TimerSlackIovec>, Errno> {
    if count > libc::UIO_MAXIOV as usize {
        return Err(Errno::EINVAL);
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let address = address.ok_or(Errno::EFAULT)?;
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        count
    ];
    memory.read_values(address, &mut iovecs)?;
    let total = iovecs.iter().try_fold(0_usize, |total, iovec| {
        total.checked_add(iovec.iov_len).ok_or(Errno::EINVAL)
    })?;
    if total > isize::MAX as usize {
        return Err(Errno::EINVAL);
    }
    Ok(iovecs
        .into_iter()
        .map(|iovec| TimerSlackIovec {
            base: iovec.iov_base as usize,
            len: iovec.iov_len,
        })
        .collect())
}

fn copy_timer_slack_output<M: MemoryAccess>(
    memory: &mut M,
    destination: Option<AddrMut<'_, u8>>,
    bytes: &[u8],
) -> Result<usize, Errno> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let copied = memory.write(destination.ok_or(Errno::EFAULT)?, bytes)?;
    if copied == 0 {
        Err(Errno::EFAULT)
    } else {
        Ok(copied)
    }
}

/// Capacity used for pipes that Detcore makes physically nonblocking.
///
/// Linux normally creates 64-KiB pipes on this platform, but silently falls back to two pages
/// once the creating UID crosses `pipe-user-pages-soft`. That host-global accounting can change
/// between the two executions of `hermit run --verify`, changing whether the same write succeeds
/// immediately or enters the scheduler's `InternalIOPolling` retry path. Two pages is the
/// pressure-mode capacity on supported x86-64 Linux hosts and, unlike 64 KiB, never requires an
/// unprivileged capacity increase while the soft limit is active.
///
/// This is also the ceiling the guest is allowed to raise a pipe to, and the
/// value `/proc/sys/fs/pipe-max-size` reports. Those three must be ONE constant:
/// a pinned capacity, an advertised maximum and an enforced maximum that can
/// drift apart are three copies of one rule, and a duplicated rule drifts in
/// N-1 places while each copy looks right on its own.
pub(crate) const DETERMINISTIC_PIPE_CAPACITY_BYTES: i32 = 8 * 1024;

/// Whether a guest's `F_SETPIPE_SZ` request must be refused as a growth past
/// the deterministic ceiling.
///
/// Separated from the handler so the BOUNDARY is testable without a guest. The
/// boundary is the whole content of this rule: a request for exactly the pinned
/// capacity must be allowed, or hermit's own pin value becomes unreachable to a
/// guest that reads `/proc/sys/fs/pipe-max-size` and asks for precisely what it
/// was told.
pub(crate) fn pipe_capacity_request_exceeds_ceiling(requested: i32) -> bool {
    requested > DETERMINISTIC_PIPE_CAPACITY_BYTES
}

/// Why the pin failed, and which descriptors Linux had already created when it did.
///
/// `pipe2` has already SUCCEEDED by the time the capacity is pinned, so the two descriptors
/// exist in the guest whatever happens next. Carrying them alongside the errno is what makes
/// releasing them possible; classifying separately from acting on it is what makes the
/// classification unit-testable without a guest.
#[derive(Debug, PartialEq, Eq)]
struct PipeCapacityFailure {
    created_fds: [i32; 2],
    error: Errno,
}

impl PipeCapacityFailure {
    fn close_syscalls(&self) -> [syscalls::Close; 2] {
        self.created_fds
            .map(|fd| syscalls::Close::new().with_fd(fd))
    }
}

/// Classify the result of pinning a pipe's capacity.
///
/// The ONLY success shape is Linux returning exactly the requested capacity. `F_SETPIPE_SZ`
/// returns the capacity it actually applied, and it may round; a rounded value is a pipe whose
/// size we did not choose, which is the host-dependent capacity this path exists to remove. It
/// is not a kernel errno, so it is reported as `EIO` rather than dressed up as one.
fn pipe_capacity_failure(
    created_fds: [i32; 2],
    capacity_result: Result<i64, Errno>,
) -> Option<PipeCapacityFailure> {
    let error = match capacity_result {
        Ok(applied) if applied == i64::from(DETERMINISTIC_PIPE_CAPACITY_BYTES) => return None,
        Ok(_) => Errno::EIO,
        Err(error) => error,
    };
    Some(PipeCapacityFailure { created_fds, error })
}

fn should_tag_sabre_internal_pipe_io(
    discovers_live_metadata: bool,
    fd_type: FdType,
    physically_nonblocking: bool,
    logically_nonblocking: bool,
) -> bool {
    discovers_live_metadata
        && fd_type == FdType::Pipe
        && physically_nonblocking
        && !logically_nonblocking
}

fn random_device_lseek_result(status_flags: i32, whence: Whence) -> Result<i64, Errno> {
    if status_flags & OFlag::O_PATH.bits() != 0 {
        return Err(Errno::EBADF);
    }
    match whence {
        Whence::SEEK_SET
        | Whence::SEEK_CUR
        | Whence::SEEK_END
        | Whence::SEEK_DATA
        | Whence::SEEK_HOLE => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

/// Inherited container output is a stream even when an outer runner stores it
/// in a seekable file.  The backing file also carries Hermit's own diagnostics,
/// so exposing its live offset makes tool logging guest-visible.  Preserve real
/// file semantics after a guest replaces stdout/stderr: `dup2(file, 1)` copies
/// the file's resource rather than this container-output resource.
fn is_inherited_container_output(resource: Option<ResourceID>) -> bool {
    matches!(
        resource,
        Some(ResourceID::Device(
            Device::ContainerStdout | Device::ContainerStderr
        ))
    )
}

fn unix_autobind_addrlen() -> i32 {
    (std::mem::offset_of!(libc::sockaddr_un, sun_path) + UNIX_AUTOBIND_NAME_LEN) as i32
}

fn unix_autobind_address(port: u16) -> libc::sockaddr_un {
    // Linux autobind names are a leading NUL followed by five lowercase hex digits.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, source) in address.sun_path[1..UNIX_AUTOBIND_NAME_LEN]
        .iter_mut()
        .zip(format!("{port:05x}").bytes())
    {
        *destination = source as libc::c_char;
    }
    address
}

// TODO-HUMAN-REVIEW(PR-904): Review the TCP_INFO compatibility boundary.
/// Retain the logical TCP state and negotiated option header while hiding all
/// host timing, rate, packet, and byte counters.
fn canonicalize_tcp_info(info: &mut [u8]) {
    for (offset, byte) in info.iter_mut().enumerate() {
        if !matches!(offset, 0 | 1 | 5 | 6) {
            *byte = 0;
        }
    }
}

// Hermit exposes exactly one isolated guest network namespace.
const DETERMINISTIC_NETNS_COOKIE: u64 = 1;

// Above Linux's PID range and below the high-bit IDs used by kernel autobind.
const DETERMINISTIC_NETLINK_PORT_ID_BASE: u32 = 0x4000_0000;

/// The path the kernel resolved an open descriptor to, read from the guest's
/// own `/proc/<pid>/fd/<fd>` link. This is the evidence authority for "which
/// object was opened": it is produced by the kernel from the descriptor itself,
/// so it is independent of the pathname spelling the guest used.
///
/// Used ONLY as a fallback for a pathname that does not classify on its own
/// (see the call site): a spelling that already classifies must keep its own
/// classification, because `/proc/self/...` and `/proc/thread-self/...` are
/// defined by the spelling and resolve to a different numeric path.
///
/// `None` when the link cannot be read (the descriptor is gone, or procfs is
/// unavailable), in which case the lexical result stands unchanged.
fn resolved_open_path(pid: i32, fd: RawFd) -> Option<PathBuf> {
    let link = std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()?;
    // A deleted or anonymous target is not a stable object name.
    link.is_absolute().then_some(link)
}

/// Resolve an `AT_FDCWD`-relative spelling in the guest's filesystem view.
///
/// Replayer chroots the guest, so the tracer-visible cwd includes the replay
/// root. Stripping `/proc/<pid>/root` produces the same guest-absolute path in
/// record and replay without depending on the opened descriptor (which is an
/// eventfd placeholder during replay).
fn resolved_at_fdcwd_path(pid: i32, path: &Path) -> Option<PathBuf> {
    debug_assert!(!path.is_absolute());
    let root = std::fs::read_link(format!("/proc/{pid}/root")).ok()?;
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let guest_cwd = cwd.strip_prefix(root).ok()?;
    Some(Path::new("/").join(guest_cwd).join(path))
}

impl<T: RecordOrReplay> Detcore<T> {
    async fn observe_timer_slack_identity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target: i32,
    ) -> Result<Option<(u64, u64)>, Error> {
        // Re-resolve the numeric proc path on every operation. Linux gives a
        // recycled TID a different proc inode, while the original open file
        // description remains bound to the exited task's inode.
        let path = format!("/proc/{target}/timerslack_ns");
        let path_bytes = path.as_bytes();
        let mut path_buffer = [0_u8; 64];
        assert!(path_bytes.len() < path_buffer.len());
        path_buffer[..path_bytes.len()].copy_from_slice(path_bytes);

        let mut stack = guest.stack().await;
        let path_address = stack.push(path_buffer).cast::<libc::c_char>();
        let statptr = StatPtr(stack.reserve());
        let stack_guard = stack.commit()?;
        let call = syscalls::Fstatat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(PathPtr::from_ptr(
                path_address.as_raw() as *const libc::c_char
            ))
            .with_stat(Some(statptr))
            .with_flags(AtFlags::empty());
        let mut identity = match guest.inject_with_retry(call).await {
            Ok(_) => {
                let stat = statptr.read(&guest.memory())?;
                Some((stat.st_dev, stat.st_ino))
            }
            Err(Errno::ENOENT) | Err(Errno::ESRCH) => None,
            Err(error) => return Err(error.into()),
        };
        drop(stack_guard);
        // Replayer runs the guest in a filesystem chroot whose `/proc` path is
        // intentionally absent, while the tracing process remains in the same
        // PID namespace and can resolve the task through its own proc mount.
        // Use that equivalent view only for record/replay; other backends keep
        // the guest-path result above as their sole authority.
        if identity.is_none()
            && guest.config().recordreplay_modes
            && let Ok(metadata) = std::fs::metadata(path)
        {
            identity = Some((metadata.dev(), metadata.ino()));
        }
        Ok(identity)
    }

    async fn require_current_timer_slack_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        binding: TimerSlackBinding,
    ) -> Result<(), Error> {
        let observed_identity = self
            .observe_timer_slack_identity(guest, binding.target)
            .await?;
        let current = guest.inject(syscalls::Gettid::new()).await? as i32;
        classify_timer_slack_binding(binding, observed_identity, current).map_err(Into::into)
    }

    fn timer_slack_binding<G: Guest<Self>>(
        &self,
        guest: &G,
        fd: RawFd,
    ) -> Result<Option<TimerSlackBinding>, Errno> {
        guest.thread_state().with_detfd(fd, |detfd| {
            detfd
                .procfs_timer_slack_binding()
                .map(|(target, device, inode)| TimerSlackBinding {
                    target,
                    device,
                    inode,
                })
        })
    }

    fn require_timer_slack_access<G: Guest<Self>>(
        &self,
        guest: &G,
        fd: RawFd,
        write: bool,
    ) -> Result<(), Errno> {
        guest.thread_state().with_detfd(fd, |detfd| {
            let flags = detfd.status_flags();
            let mode = flags & libc::O_ACCMODE;
            let denied = flags & libc::O_PATH != 0
                || if write {
                    mode == libc::O_RDONLY
                } else {
                    mode == libc::O_WRONLY
                };
            (!denied).then_some(()).ok_or(Errno::EBADF)
        })?
    }

    fn read_timer_slack_input<G: Guest<Self>>(
        &self,
        guest: &G,
        buffer: Option<Addr<u8>>,
        count: usize,
    ) -> Result<u64, Errno> {
        let mut bytes = vec![0_u8; count.min(TIMER_SLACK_PARSE_BYTES)];
        if !bytes.is_empty() {
            guest
                .memory()
                .read_exact(buffer.ok_or(Errno::EFAULT)?, &mut bytes)?;
        }
        parse_timer_slack_write(&bytes)
    }

    async fn read_timer_slack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: Option<AddrMut<'_, u8>>,
        maximum: usize,
    ) -> Result<i64, Error> {
        self.require_timer_slack_access(guest, fd, false)?;
        if maximum == 0 {
            return Ok(0);
        }
        let binding = self
            .timer_slack_binding(guest, fd)?
            .expect("timer-slack read lost its procfs classification");
        self.require_current_timer_slack_target(guest, binding)
            .await?;
        let value = guest.thread_state().timer_slack_ns;
        let preview = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.preview_procfs_timer_slack(value, maximum))?
            .expect("timer-slack procfs state disappeared");
        let copied = copy_timer_slack_output(&mut guest.memory(), buffer, &preview.bytes)?;
        if copied != 0 {
            guest.thread_state().with_detfd(fd, |detfd| {
                detfd.commit_procfs_timer_slack_read(&preview, copied);
            })?;
        }
        Ok(copied as i64)
    }

    async fn pread_timer_slack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: Option<AddrMut<'_, u8>>,
        maximum: usize,
        offset: i64,
    ) -> Result<i64, Error> {
        if offset < 0 {
            return Err(Errno::EINVAL.into());
        }
        self.require_timer_slack_access(guest, fd, false)?;
        if maximum == 0 {
            return Ok(0);
        }
        let binding = self
            .timer_slack_binding(guest, fd)?
            .expect("timer-slack pread lost its procfs classification");
        self.require_current_timer_slack_target(guest, binding)
            .await?;
        let value = guest.thread_state().timer_slack_ns;
        let bytes = guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                detfd.take_procfs_timer_slack_at(value, offset as usize, maximum)
            })?
            .expect("timer-slack procfs state disappeared");
        Ok(copy_timer_slack_output(&mut guest.memory(), buffer, &bytes)? as i64)
    }

    async fn readv_timer_slack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        iovecs: Vec<TimerSlackIovec>,
        offset: Option<i64>,
        flags: i32,
    ) -> Result<i64, Error> {
        self.require_timer_slack_access(guest, fd, false)?;
        let maximum = iovecs.iter().map(|iovec| iovec.len).sum::<usize>();
        if maximum == 0 {
            return Ok(0);
        }
        if flags & !libc::RWF_HIPRI != 0 {
            return Err(Errno::EOPNOTSUPP.into());
        }
        let binding = self
            .timer_slack_binding(guest, fd)?
            .expect("timer-slack readv lost its procfs classification");
        self.require_current_timer_slack_target(guest, binding)
            .await?;
        let value = guest.thread_state().timer_slack_ns;
        let mut positioned_offset = offset.map(|offset| offset as usize);
        let mut total = 0_usize;
        for iovec in iovecs {
            if iovec.len == 0 {
                continue;
            }
            let sequential_preview = if positioned_offset.is_none() {
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.preview_procfs_timer_slack(value, iovec.len)
                })?
            } else {
                None
            };
            let bytes = match (&sequential_preview, positioned_offset) {
                (Some(preview), None) => preview.bytes.clone(),
                (None, Some(offset)) => guest
                    .thread_state()
                    .with_detfd(fd, |detfd| {
                        detfd.take_procfs_timer_slack_at(value, offset, iovec.len)
                    })?
                    .expect("timer-slack procfs state disappeared"),
                _ => unreachable!("timer-slack read mode changed while reading"),
            };
            if bytes.is_empty() {
                break;
            }
            let copied = match copy_timer_slack_output(
                &mut guest.memory(),
                AddrMut::from_raw(iovec.base),
                &bytes,
            ) {
                Ok(copied) => copied,
                Err(_) if total > 0 => return Ok(total as i64),
                Err(error) => return Err(error.into()),
            };
            if let Some(preview) = &sequential_preview {
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.commit_procfs_timer_slack_read(preview, copied);
                })?;
            }
            total += copied;
            if let Some(offset) = positioned_offset.as_mut() {
                *offset += copied;
            }
            if copied != bytes.len() {
                return Ok(total as i64);
            }
            if bytes.len() != iovec.len {
                break;
            }
        }
        Ok(total as i64)
    }

    async fn write_timer_slack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: Option<Addr<'_, u8>>,
        count: usize,
    ) -> Result<i64, Error> {
        self.require_timer_slack_access(guest, fd, true)?;
        let requested = self.read_timer_slack_input(guest, buffer, count)?;
        let binding = self
            .timer_slack_binding(guest, fd)?
            .expect("timer-slack write lost its procfs classification");
        self.require_current_timer_slack_target(guest, binding)
            .await?;
        let state = guest.thread_state_mut();
        state.timer_slack_ns = if requested == 0 {
            state.default_timer_slack_ns
        } else {
            requested
        };
        i64::try_from(count).map_err(|_| Errno::EINVAL.into())
    }

    async fn writev_timer_slack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        iovecs: Vec<TimerSlackIovec>,
        flags: i32,
    ) -> Result<i64, Error> {
        self.require_timer_slack_access(guest, fd, true)?;
        if iovecs.iter().all(|iovec| iovec.len == 0) {
            return Ok(0);
        }
        if flags & !libc::RWF_HIPRI != 0 {
            return Err(Errno::EOPNOTSUPP.into());
        }
        // Procfs supplies only `.write`, so Linux's writev fallback invokes it
        // once per nonempty iovec. Preserve its partial-success behavior and
        // let the last successful segment determine the current slack.
        let mut total = 0_i64;
        for iovec in iovecs {
            if iovec.len == 0 {
                continue;
            }
            let buffer = Addr::from_raw(iovec.base).ok_or(Errno::EFAULT);
            let requested = match buffer
                .and_then(|buffer| self.read_timer_slack_input(guest, Some(buffer), iovec.len))
            {
                Ok(requested) => requested,
                Err(_error) if total > 0 => return Ok(total),
                Err(error) => return Err(error.into()),
            };
            let binding = self
                .timer_slack_binding(guest, fd)?
                .expect("timer-slack writev lost its procfs classification");
            if let Err(error) = self
                .require_current_timer_slack_target(guest, binding)
                .await
            {
                return if total > 0 { Ok(total) } else { Err(error) };
            }
            let state = guest.thread_state_mut();
            state.timer_slack_ns = if requested == 0 {
                state.default_timer_slack_ns
            } else {
                requested
            };
            total = total
                .checked_add(i64::try_from(iovec.len).map_err(|_| Errno::EINVAL)?)
                .ok_or(Errno::EINVAL)?;
        }
        Ok(total)
    }
    /// Inject an extra fstat to retrieve file metadata.
    pub(crate) async fn inject_fstat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        raw_fd: RawFd,
    ) -> Result<libc::stat, Errno> {
        info!(
            "Injecting additional fstat to retrieve file metadata on fd {}.",
            raw_fd
        );
        let mut stack = guest.stack().await;
        let statptr: StatPtr = StatPtr(stack.reserve());
        stack.commit()?;

        // NOTE: Must retry the injection here. This could get interrupted and
        // we don't want to rerun the entire syscall handler twice.
        guest
            .inject_with_retry(Syscall::Fstat(
                syscalls::Fstat::new()
                    .with_fd(raw_fd)
                    .with_stat(Some(statptr)),
            ))
            .await?;

        let copied = statptr.read(&guest.memory())?;
        // clear stack memory used for fstat allocation
        guest
            .memory()
            .write_exact(statptr.0.cast(), &[0; std::mem::size_of::<libc::stat>()])?;
        trace!("extra fstat returned inode {}", copied.st_ino);
        Ok(copied)
    }

    // helper function to track a new file descriptor.
    pub(crate) async fn add_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        flags: OFlag,
        ty: FdType,
    ) -> Result<(), Errno> {
        let stat = if guest.config().virtualize_metadata {
            Some(self.inject_fstat(guest, fd).await?.into())
        } else {
            None
        };
        guest.thread_state().add_fd(fd, flags, ty, stat)
    }

    pub(crate) async fn release_port_for_open_file<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file_id: OpenFileId,
    ) -> Option<u16> {
        let response = send_and_update_time(guest, GlobalRequest::ReleasePort(open_file_id)).await;
        match response.1 {
            GlobalResponse::ReleasePort(port) => port,
            other => panic!("unexpected release-port response: {other:?}"),
        }
    }

    pub(crate) async fn restore_port_for_open_file<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file_id: OpenFileId,
        port: u16,
    ) {
        let response =
            send_and_update_time(guest, GlobalRequest::AddUsedPort(port, open_file_id)).await;
        match response.1 {
            GlobalResponse::AddUsedPort => {}
            other => panic!("unexpected restore-port response: {other:?}"),
        }
    }

    /// Openat system call.
    pub async fn handle_openat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
    ) -> Result<i64, Error> {
        let path = call.path().ok_or(Errno::EFAULT)?;
        let path: PathBuf = path.read(&guest.memory())?;
        // A relative spelling is not the object. `chdir("/sys/module/kvm");
        // open("refcnt")` and an absolute open name the SAME kernel object, so
        // classifying the unresolved lexical pathname lets one spelling bypass
        // normalization and expose the host value. Absolute paths are already
        // bound; a dirfd supplies its own prefix; AT_FDCWD-relative spellings
        // are resolved through the guest's root and cwd before the open so the
        // result does not depend on Replayer's placeholder descriptor.
        let observed_path = if path.is_absolute() {
            path.clone()
        } else if call.dirfd() == libc::AT_FDCWD {
            resolved_at_fdcwd_path(guest.pid().as_raw(), &path).unwrap_or_else(|| path.clone())
        } else {
            guest
                .thread_state()
                .with_detfd(call.dirfd(), |detfd| detfd.path())?
                .map_or_else(|| path.clone(), |directory| directory.join(&path))
        };

        let resource = ResourceID::Path(path.clone());
        // Ask for permission to resolve this path into a file:
        let request = guest.thread_state().mk_request(resource, Permission::R);
        resource_request(guest, request).await;
        let res = self.record_or_replay(guest, Syscall::Openat(call)).await;

        match res {
            Ok(fd) => {
                let fd = fd as RawFd;
                let fd_type = path.to_str().map_or(FdType::Regular, |fname| {
                    if fname == "/dev/random" || fname == "/dev/urandom" {
                        FdType::Rng
                    } else {
                        FdType::Regular
                    }
                });
                self.add_fd(guest, fd, call.flags(), fd_type).await?;
                // Classify the spelling the guest used FIRST. Several kinds are
                // defined by that spelling and MUST keep it: `/proc/self/...`,
                // `/proc/thread-self/...` and the mountinfo aliases all resolve
                // through `/proc/<pid>/fd/<fd>` to a numeric `/proc/<pid>/...`
                // path, which is a DIFFERENT (or absent) classification. So
                // resolution must never overwrite a spelling that already
                // classifies.
                //
                // Only when the spelling yields nothing do we ask the kernel
                // what the descriptor actually names. That is exactly the
                // AT_FDCWD/alias gap -- `chdir("/sys/module/kvm"); open("refcnt")`
                // classifies as nothing lexically -- and scoping it this way
                // makes the fallback MONOTONE: it can only add a classification
                // where there was none, never change one that already existed.
                let mut procfs = ProcfsFile::from_path(&observed_path).or_else(|| {
                    resolved_open_path(guest.pid().as_raw(), fd)
                        .filter(|resolved| resolved != &observed_path)
                        .and_then(|resolved| ProcfsFile::from_path(&resolved))
                });
                if procfs
                    .as_ref()
                    .is_some_and(ProcfsFile::needs_bound_thread_identity)
                {
                    // TODO-HUMAN-REVIEW(PR-964): Bind thread-self at open time,
                    // matching procfs inode resolution even if another thread or
                    // a forked process later reads the shared descriptor.
                    let tgid = guest.inject(syscalls::Getpid::new()).await? as i32;
                    let tid = guest.inject(syscalls::Gettid::new()).await? as i32;
                    let ppid = guest.inject(syscalls::Getppid::new()).await? as i32;
                    procfs
                        .as_mut()
                        .expect("thread identity request lost its procfs file")
                        .bind_thread_identity(tgid, tid, ppid);
                }
                if procfs
                    .as_ref()
                    .and_then(ProcfsFile::timer_slack_target)
                    .is_some()
                {
                    let target = procfs
                        .as_ref()
                        .and_then(ProcfsFile::timer_slack_target)
                        .expect("timer-slack target disappeared");
                    let stat = match guest.thread_state().with_detfd(fd, |detfd| detfd.stat())? {
                        Some(stat) => libc::stat::from(&stat),
                        None => self.inject_fstat(guest, fd).await?,
                    };
                    // Recorder opens a real proc inode, while Replayer reserves
                    // the recorded descriptor number with an anonymous eventfd.
                    // A real proc descriptor is the strongest open-time task
                    // incarnation witness. For a virtual replay descriptor,
                    // bind the live numeric proc path instead; every operation
                    // re-resolves that same path, so exit or TID reuse still
                    // changes the inode and returns ESRCH.
                    let identity = if stat.st_mode & libc::S_IFMT == libc::S_IFREG {
                        (stat.st_dev, stat.st_ino)
                    } else {
                        self.observe_timer_slack_identity(guest, target)
                            .await?
                            .unwrap_or((stat.st_dev, stat.st_ino))
                    };
                    procfs
                        .as_mut()
                        .expect("timer-slack classification disappeared")
                        .bind_timer_slack_identity(identity.0, identity.1);
                }
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_path(&observed_path);
                    if let Some(procfs) = procfs.clone() {
                        detfd.set_procfs(procfs);
                    }
                })?;
                resource_release_all(guest).await;
                Ok(fd as i64)
            }
            // TODO: audit for error-nondeterminism:
            Err(e) => {
                resource_release_all(guest).await;
                Err(e.into())
            }
        }
    }

    /// SYS_close system call.
    pub async fn handle_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Close,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let res = self.record_or_replay(guest, call).await;
        let fd_was_released = !matches!(res, Err(Errno::EBADF) | Err(Errno::ERESTARTSYS));
        if fd_was_released {
            if let Some(open_file_id) = guest.thread_state_mut().remove_fd(fd) {
                self.release_port_for_open_file(guest, open_file_id).await;
            }
            trace!("Closed {}", fd);
        }
        res.map_err(Error::from)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-838): Review close_range descriptor-table synchronization.
    /// Close a contiguous descriptor range and mirror successful closes in Detcore.
    ///
    /// The pinned Reverie revision exposes close_range as `Syscall::Other`. The
    /// common flags=0 operation cannot block and is deterministic for the
    /// process-local descriptor table. CLOSE_RANGE_UNSHARE and
    /// CLOSE_RANGE_CLOEXEC need separate shared-table modeling, so return ENOSYS
    /// for nonzero flags rather than letting strict execution silently diverge.
    pub async fn handle_close_range<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let Syscall::Other(_, args) = call else {
            unreachable!("close_range unexpectedly gained a typed variant")
        };
        let first = args.arg0 as u32;
        let last = args.arg1 as u32;
        let flags = args.arg2 as u32;
        if flags != 0 {
            return Err(Errno::ENOSYS.into());
        }

        let result = self.record_or_replay(guest, call).await;
        if result.is_ok() {
            let released = guest.thread_state_mut().remove_fd_range(first, last);
            for open_file_id in released {
                self.release_port_for_open_file(guest, open_file_id).await;
            }
        }
        result.map_err(Error::from)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// Advisory whole-file locks, forwarded to the kernel.
    ///
    /// This was previously an unconditional no-op success, justified by the
    /// claim that "an advisory whole-file lock is never contended within the
    /// serialized container". That is false: serializing guest threads stops
    /// them EXECUTING simultaneously, it does not stop their lock HOLD
    /// INTERVALS from overlapping. A holder that is descheduled -- because it
    /// blocked, forked, or simply used up its timeslice -- keeps holding while
    /// another process runs and observes the lock. Measured before this change,
    /// on both ptrace and DBI, two processes held the same `LOCK_EX`
    /// simultaneously while native correctly returned `EWOULDBLOCK`.
    ///
    /// A no-op is the wrong failure direction for a determinism tool. It is
    /// deterministically wrong, so double-run verification cannot see it, and it
    /// silently removes mutual exclusion from every guest that uses a lockfile.
    ///
    /// Forwarding is what `fcntl` already does for POSIX record locks, which is
    /// why those work. The guest's descriptor is a real host descriptor, so the
    /// kernel supplies the whole contract for free and consistently with itself:
    /// shared vs exclusive, `LOCK_NB`, upgrade/downgrade (which Linux performs
    /// non-atomically -- see below, this handler compensates), release on
    /// `LOCK_UN`, release when the last descriptor for the open file
    /// description is closed, and release on process exit.
    ///
    /// Determinism, scoped to what is actually true. When every contender is
    /// inside the container the outcome is a function of which guest holds the
    /// lock, and that is fixed by Detcore's deterministic schedule, so a given
    /// program and seed produce the same acquisition outcome every run.
    ///
    /// The scope is not decoration. Because this forwards to the kernel, a
    /// process OUTSIDE the container holding a lock on a guest-visible file
    /// does change the guest's result -- measured: with a host `flock -x`
    /// holder, a guest `LOCK_EX|LOCK_NB` returns `EWOULDBLOCK`, and acquires
    /// without one. That is a host-state leak, it is faithful to Linux, and it
    /// is the same leak `fcntl` record locks have always had here. Hermit
    /// already declines to make a mutating external filesystem deterministic,
    /// and lock state on a shared file is part of that state. Do not restate
    /// this as "no host state enters the decision": it does, and the previous
    /// bug in this very function came from writing down a determinism argument
    /// that was broader than the truth.
    ///
    /// Note that the no-op this replaced was not host-independent in any useful
    /// sense either -- it was host-independent by being wrong in all cases.
    ///
    /// # Why a blocking request is probed non-blockingly, and what that costs
    ///
    /// A guest thread parked inside a kernel `flock` is not visible to the
    /// deterministic scheduler as blocked, so nothing runs to release the lock
    /// and the whole container wedges -- measured: a four-way contention guest
    /// that completes natively hung indefinitely under a plain forwarding
    /// implementation. So a blocking operation is rewritten to `LOCK_NB` and,
    /// if it turns out to be contended, refused rather than hung.
    ///
    /// That rewrite is not free, and the cost is a *lock the guest already
    /// owns*. Linux converts an `flock` lock in place and the conversion is not
    /// atomic: `flock_lock_inode` deletes this open file description's existing
    /// lock **before** it scans for a conflict, so a contended `LOCK_SH` ->
    /// `LOCK_EX` conversion leaves the caller holding nothing and then reports
    /// `EWOULDBLOCK`. Natively the guest never observes that intermediate
    /// state, because a *blocking* request would sleep and eventually acquire.
    /// Under the rewrite it would: the guest asked to wait, got told "no", and
    /// silently lost the shared lock it was already relying on.
    ///
    /// So this handler restores the prior mode before refusing a *blocking*
    /// conversion, making the refusal side-effect-free. It deliberately does
    /// **not** restore when the guest itself passed `LOCK_NB`: there the drop is
    /// exactly what Linux does, and re-acquiring would be a divergence in the
    /// other direction. `DetFd::flock_mode` is what makes the two cases
    /// distinguishable -- it records the mode Detcore last saw the kernel grant
    /// for this open file description, so a first acquisition (nothing to lose)
    /// is not confused with a conversion (something to lose).
    ///
    /// That cache covers locks Detcore granted while it had sole knowledge of
    /// the open file description. State becomes permanently unknown when the
    /// descriptor is inherited across a process fork, discovered after tracing
    /// begins, or received through `SCM_RIGHTS`, because another process can
    /// change that shared kernel lock without updating this cache. A blocking
    /// conversion in unknown state is refused before the nonblocking probe, so
    /// the refusal cannot destroy a lock Detcore cannot restore.
    pub async fn handle_flock<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Flock,
    ) -> Result<i64, Error> {
        const LOCK_NB: i32 = libc::LOCK_NB;
        /// `LOCK_SH`/`LOCK_EX`/`LOCK_UN` with `LOCK_NB` (and any padding) masked off.
        const MODE_MASK: i32 = libc::LOCK_SH | libc::LOCK_EX | libc::LOCK_UN;

        let (fd, operation) = (call.fd(), call.operation());
        let requested = operation & MODE_MASK;
        let caller_wants_nonblocking = operation & LOCK_NB != 0;
        let releasing = requested == libc::LOCK_UN;
        let valid_operation = operation & !(MODE_MASK | LOCK_NB) == 0
            && matches!(requested, libc::LOCK_SH | libc::LOCK_EX | libc::LOCK_UN);
        let dettid = guest.thread_state().dettid;

        // Preserve kernel validation for malformed operations. In particular,
        // an unknown descriptor with an invalid mode must report EINVAL rather
        // than being mistaken for a valid blocking request and refused with
        // ENOLCK.
        if !valid_operation {
            return self
                .record_or_replay(guest, call)
                .await
                .map_err(Error::from);
        }

        // The mode Detcore last saw the kernel grant this open file description.
        let known_held = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.known_flock_mode())
            .unwrap_or(None);

        // A nonblocking conversion is allowed to have Linux's documented
        // non-atomic side effect. A blocking conversion is not: if this open
        // file description was inherited or received and its prior mode is
        // unknown, probing could silently drop a lock we cannot restore.
        // Validate the descriptor without changing lock state, then refuse the
        // uncertain blocking operation before issuing flock at all.
        if !caller_wants_nonblocking && !releasing && known_held.is_none() {
            guest
                .inject_with_retry(Syscall::Fcntl(
                    syscalls::Fcntl::new().with_fd(fd).with_cmd(F_GETFD),
                ))
                .await?;
            error!(
                "[dtid {dettid}] blocking flock(fd={fd}, operation={operation:#x}) refused: \
                 this open file description existed before Detcore observed its lock state, \
                 so a nonblocking probe could destroy a lock that cannot be restored. Use \
                 LOCK_NB, or run without --strict to receive ENOLCK."
            );
            return self
                .refuse_unserviceable_operation(guest, Sysno::flock, Errno::ENOLCK)
                .await;
        }
        let held = known_held.flatten();

        // LOCK_UN cannot block, so it is forwarded exactly as the guest wrote it.
        let probe_operation = if releasing {
            operation
        } else {
            operation | LOCK_NB
        };
        let result = self
            .record_or_replay(guest, call.with_operation(probe_operation))
            .await;

        match result {
            Ok(value) => {
                let granted = if releasing { None } else { Some(requested) };
                let _ = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.set_flock_mode(granted));
                trace!(
                    "flock(fd={}, operation={:#x}) served, open file now holds {:?}",
                    fd, operation, granted
                );
                Ok(value)
            }
            Err(Errno::EWOULDBLOCK) if caller_wants_nonblocking => {
                // Exactly what the guest asked for, including Linux's own
                // non-atomic conversion behavior: if this was a conversion, the
                // kernel really did drop the prior lock on the way to failing,
                // so the cache must forget it rather than claim a lock the
                // guest no longer holds.
                if held.is_some_and(|held| held != requested) {
                    let _ = guest
                        .thread_state()
                        .with_detfd(fd, |detfd| detfd.set_flock_mode(None));
                }
                trace!("flock(fd={}, operation={:#x}) would block", fd, operation);
                Err(Errno::EWOULDBLOCK.into())
            }
            Err(Errno::EWOULDBLOCK) => {
                // The guest asked to wait, and Detcore substituted a probe.
                // Undo the probe's collateral damage before refusing.
                if let Some(previous) = held.filter(|previous| *previous != requested) {
                    let restore = call.with_operation(previous | LOCK_NB);
                    match self.record_or_replay(guest, restore).await {
                        Ok(_) => {
                            warn!(
                                "[dtid {dettid}] contended blocking flock(fd={fd}, \
                                 operation={operation:#x}) refused; restored this open file's \
                                 prior {previous:#x} lock, which Linux's non-atomic conversion \
                                 had dropped. The guest holds exactly what it held before the \
                                 call."
                            );
                        }
                        Err(err) => {
                            let _ = guest
                                .thread_state()
                                .with_detfd(fd, |detfd| detfd.set_flock_mode(None));
                            error!(
                                "[dtid {dettid}] contended blocking flock(fd={fd}, \
                                 operation={operation:#x}) refused, AND this open file's prior \
                                 {previous:#x} lock could not be restored ({err}). Linux's \
                                 non-atomic conversion dropped it and something outside this \
                                 container took it in the interval. The guest has lost a lock \
                                 it held; treat any mutual exclusion it was protecting as \
                                 broken."
                            );
                        }
                    }
                }
                // Waiting faithfully needs a wait queue owned by the
                // deterministic scheduler, the way futexes are handled; until
                // that exists, refuse loudly. Returning success would recreate
                // the mutual-exclusion bug this handler was written to fix, and
                // blocking in the kernel would deadlock the container.
                error!(
                    "[dtid {dettid}] blocking flock(fd={fd}, operation={operation:#x}) is \
                     contended, and Detcore cannot yet park a thread on a file lock \
                     deterministically. Refusing rather than granting a lock another guest \
                     holds. Use LOCK_NB, or run without --strict to receive ENOLCK."
                );
                self.refuse_unserviceable_operation(guest, Sysno::flock, Errno::ENOLCK)
                    .await
            }
            Err(err) => {
                trace!(
                    "flock(fd={}, operation={:#x}) refused: {}",
                    fd, operation, err
                );
                Err(err.into())
            }
        }
    }

    async fn snapshot_procfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<Vec<u8>, Error> {
        const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

        // A backend-owned read may have advanced the kernel cursor without
        // passing through Detcore's logical procfs cursor (KVM does this for
        // worker-shared descriptors). Rewind before taking the initial snapshot
        // so a later intercepted pread cannot snapshot from EOF.
        //
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#1903): ESPIPE is not a failure here. Several procfs
        // files are legitimately non-seekable -- `/proc/net/*` single-release
        // seq_files return ESPIPE from `llseek` on the host, verified natively:
        // `lseek(fd, 0, SEEK_SET)` on `/proc/net/sockstat` gives ESPIPE while the
        // subsequent `read(2)` returns data. Propagating that ESPIPE made the
        // GUEST's `read` fail on a file Linux reads fine (`cat
        // /proc/net/sockstat` -> "Illegal seek"), which is a deviation from Linux
        // semantics, not a determinism requirement: the rewind is an internal
        // correction Detcore performs for its own benefit and the guest never
        // asked for it. A non-seekable fd also cannot have been advanced behind
        // our back by a seek, and a freshly opened one is already at offset 0, so
        // skipping the rewind loses nothing the rewind was protecting.
        match guest
            .inject_with_retry(Syscall::Lseek(
                syscalls::Lseek::new()
                    .with_fd(call.fd())
                    .with_offset(0)
                    .with_whence(Whence::SEEK_SET),
            ))
            .await
        {
            Ok(_) => {}
            Err(Errno::ESPIPE) => {}
            Err(err) => return Err(err.into()),
        }

        let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
        let mut contents = Vec::new();
        loop {
            let bytes_read = self.record_or_replay(guest, call).await? as usize;
            if bytes_read == 0 {
                return Ok(contents);
            }
            if contents.len() + bytes_read > MAX_SNAPSHOT_BYTES {
                return Err(Errno::EFBIG.into());
            }

            let mut chunk = vec![0; bytes_read];
            guest.memory().read_exact(remote_buf, &mut chunk)?;
            contents.extend_from_slice(&chunk);
        }
    }

    async fn initialize_procfs_snapshot<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<(), Error> {
        let contents = self.snapshot_procfs(guest, call).await?;
        let virtual_uptime_seconds = self.calculate_uptime(guest).await?;
        let virtual_realtime_seconds = i64::try_from(thread_observe_time(guest).await.as_secs())
            .map_err(|_| Errno::EOVERFLOW)?;
        // TODO-HUMAN-REVIEW(PR-863): Use configured guest memory for meminfo.
        let virtual_memory_kb = guest.config().memory / 1024;
        // TODO-HUMAN-REVIEW(PR-723): Review injected identity snapshot reads.
        let virtual_pid = guest.inject(syscalls::Getpid::new()).await? as i32;
        let virtual_ppid = guest.inject(syscalls::Getppid::new()).await? as i32;
        let virtual_pty_count = guest.thread_state().count_open_files_at_paths(&[
            std::path::Path::new("/dev/ptmx"),
            std::path::Path::new("/dev/pts/ptmx"),
        ]);
        let target_fd = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_target_fd())?;
        let fdinfo_identity = if let Some(target_fd) = target_fd {
            let (cached_stat, logical_flags, open_file_id, fd_type, inode_override) =
                guest.thread_state().with_detfd(target_fd, |detfd| {
                    (
                        detfd.stat(),
                        detfd.status_flags(),
                        detfd.open_file_id(),
                        detfd.ty(),
                        deterministic_stdio_inode_for_resource(target_fd, detfd.resource()),
                    )
                })?;
            let raw_inode = match cached_stat {
                Some(stat) => stat.inode,
                None => {
                    let stat = self.inject_fstat(guest, target_fd).await?;
                    stat.st_ino
                }
            };
            let virtual_inode = match inode_override {
                Some(inode) => inode,
                None => determinize_inode(guest, raw_inode).await.0,
            };
            let raw_mount_id =
                detcore_model::procfs::parse_fdinfo_mount_id(&contents).ok_or_else(|| {
                    Error::Tool(anyhow::anyhow!(
                        "kernel returned malformed /proc/*/fdinfo without one numeric mnt_id"
                    ))
                })?;
            // CLI container and recording paths provide the exact namespace's
            // row order. Replay intentionally retains the recording-time raw
            // IDs because ReadV2 supplies recording-time fdinfo bytes.
            let has_configured_mount_ids = guest.config().mountinfo_mount_ids_captured;
            let virtual_mount_id = if raw_mount_id == 0 {
                // Linux uses zero for anonymous objects such as memfd. Key the
                // equivalence on the observed value, not our descriptor type.
                0
            } else if !has_configured_mount_ids {
                // Public non-container callers have no pre-captured provenance.
                // Mount/unshare/setns are refused once Detcore starts, so a
                // tracer-side snapshot of this task's namespace is immutable.
                let mountinfo_path = format!("/proc/{}/mountinfo", guest.pid().as_raw());
                let mountinfo_contents = std::fs::read(&mountinfo_path).map_err(|error| {
                    Error::Tool(anyhow::anyhow!(
                        "failed to read {mountinfo_path} while validating fdinfo mnt_id: {error}"
                    ))
                })?;
                let mountinfo_rows =
                    crate::procfs::parse_mountinfo(&mountinfo_contents).ok_or_else(|| {
                        Error::Tool(anyhow::anyhow!(
                            "kernel returned malformed {mountinfo_path} while validating fdinfo mnt_id"
                        ))
                    })?;
                let snapshot = MountInfoSnapshot::new(
                    mountinfo_rows,
                    &[],
                    false,
                    BTreeMap::new(),
                    BTreeMap::new(),
                )
                .ok_or_else(|| {
                    Error::Tool(anyhow::anyhow!(
                        "{mountinfo_path} failed strict identity validation for fdinfo"
                    ))
                })?;
                determinize_mount_id(guest, raw_mount_id, Some(snapshot.raw_mount_id_order()))
                    .await
                    .ok_or_else(|| {
                        Error::Tool(anyhow::anyhow!(
                            "mountinfo mount-ID order changed after the identity snapshot while resolving fdinfo mnt_id {raw_mount_id} for {fd_type:?} from {mountinfo_path}"
                        ))
                    })?
            } else {
                determinize_mount_id(guest, raw_mount_id, None)
                    .await
                    .ok_or_else(|| {
                        Error::Tool(anyhow::anyhow!(
                            "recorded mount identity provenance was invalid while resolving fdinfo mnt_id {raw_mount_id} for {fd_type:?}"
                        ))
                    })?
            };
            Some((
                // Determinized immediately above (stdio-special or
                // `determinize_inode`); lowered to an integer only here, at the
                // point it is rendered into guest-visible fdinfo text.
                virtual_inode.as_raw(),
                logical_flags,
                open_file_id.deterministic_socket_cookie(),
                virtual_mount_id,
            ))
        } else {
            None
        };
        let needs_random_uuid = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_random_uuid())?;
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-955): Review deterministic kernel UUID generation.
        let random_uuid =
            needs_random_uuid.then(|| guest.thread_state_mut().thread_prng().random::<[u8; 16]>());
        // ⚠️ DETERMINIZE HERE, IN THE CALLER, THROUGH THE SAME POOLS `stat` USES.
        //
        // The sanitizers in `crate::procfs` are pure functions of content and
        // hold no guest handle, so they cannot reach `InodePool`/`DevicePool`.
        // Minting an identity down there would make the maps column stable and
        // STILL DISAGREE with `stat` -- deterministic, reproducible and wrong.
        // This mirrors how `fdinfo_identity` is built a few lines above:
        // determinize with `determinize_inode`/`determinize_device`, then hand
        // the finished values down purely to be rendered.
        //
        // BOTH COLUMNS, not just the inode. `determinize_stat` sanitizes
        // `st_dev` as well, so rewriting only the inode would leave the device
        // disagreeing -- the same defect with the reported symptom removed.
        let needs_mapping_identities = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_mapping_identities())?;
        let mut mapping_identities: BTreeMap<(u64, u64), (u64, u64)> = BTreeMap::new();
        if needs_mapping_identities {
            // A mapping backed by stdio must report the SAME inode fdinfo
            // reports for that fd, which is the fixed `deterministic_stdio_inode`
            // value rather than a pooled one. Matching is by raw inode, read
            // from the cached stat only: injecting an fstat here would add
            // syscalls to every maps read and perturb the very traces this
            // change is meant to keep consistent.
            let mut stdio_by_raw_inode: BTreeMap<u64, DetInode> = BTreeMap::new();
            for fd in libc::STDIN_FILENO..=libc::STDERR_FILENO {
                let cached = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| {
                        let inode = deterministic_stdio_inode_for_resource(fd, detfd.resource())?;
                        detfd.stat().map(|stat| (stat.inode, inode))
                    })
                    .ok()
                    .flatten();
                if let Some((raw, det)) = cached {
                    stdio_by_raw_inode.insert(raw, det);
                }
            }
            let raw_pairs: BTreeSet<(u64, u64)> = String::from_utf8_lossy(&contents)
                .lines()
                .filter_map(crate::procfs::mapping_header_identity)
                .collect();
            for (raw_dev, raw_inode) in raw_pairs {
                let det_inode = match stdio_by_raw_inode.get(&raw_inode) {
                    Some(inode) => *inode,
                    None => determinize_inode(guest, raw_inode).await.0,
                };
                let det_dev = determinize_device(guest, raw_dev).await;
                mapping_identities.insert((raw_dev, raw_inode), (det_dev, det_inode.as_raw()));
            }
        }
        let mountinfo = if guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_mountinfo_identities())?
        {
            let mut rows = crate::procfs::parse_mountinfo(&contents).ok_or_else(|| {
                Error::Tool(anyhow::anyhow!(
                    "kernel returned malformed /proc/*/mountinfo"
                ))
            })?;
            let mut device_rewrites = BTreeMap::new();
            for &(mountinfo_device, metadata_device) in &guest.config().mountinfo_device_rewrites {
                if device_rewrites
                    .insert(mountinfo_device, metadata_device)
                    .is_some()
                {
                    return Err(Error::Tool(anyhow::anyhow!(
                        "duplicate proven mountinfo device rewrite for raw device {mountinfo_device}"
                    )));
                }
            }
            for row in &mut rows {
                if let Some(metadata_device) = device_rewrites.get(&row.raw_device) {
                    row.raw_device = *metadata_device;
                }
            }
            let mut raw_devices = Vec::new();
            let mut seen_devices = BTreeSet::new();
            for row in &rows {
                if seen_devices.insert(row.raw_device) {
                    raw_devices.push(row.raw_device);
                }
            }
            let mut devices = BTreeMap::new();
            let virtualize_metadata = guest.config().virtualize_metadata;
            if virtualize_metadata {
                // Intentionally use snapshot row order to pre-populate the
                // same run-global DevicePool used by stat/statx. This makes
                // every later observation of a device agree within the run.
                // It does not promise that unlike host filesystem layouts
                // expose the same device equivalence classes or order.
                for raw in raw_devices {
                    devices.insert(raw, determinize_device(guest, raw).await);
                }
            }
            let mut root_rewrites = BTreeMap::new();
            for rewrite in &guest.config().mountinfo_root_rewrites {
                if root_rewrites
                    .insert(rewrite.raw_mount_id, rewrite.clone())
                    .is_some()
                {
                    return Err(Error::Tool(anyhow::anyhow!(
                        "duplicate proven mountinfo root rewrite for mount ID {}",
                        rewrite.raw_mount_id
                    )));
                }
            }
            let snapshot = MountInfoSnapshot::new(
                rows,
                if guest.config().mountinfo_mount_ids_captured {
                    &guest.config().mountinfo_mount_ids
                } else {
                    &[]
                },
                virtualize_metadata,
                devices,
                root_rewrites,
            )
            .ok_or_else(|| {
                Error::Tool(anyhow::anyhow!(
                    "mountinfo snapshot failed strict identity validation"
                ))
            })?;
            if !validate_mountinfo_identity_order(guest, snapshot.raw_mount_id_order()).await {
                return Err(Error::Tool(anyhow::anyhow!(
                    "mountinfo mount-ID order changed after the run-global identity snapshot"
                )));
            }
            Some(snapshot)
        } else {
            None
        };
        guest.thread_state().with_detfd(call.fd(), |detfd| {
            detfd.initialize_procfs(
                contents.clone(),
                ProcfsSnapshotContext {
                    mapping_identities: mapping_identities.clone(),
                    mountinfo: mountinfo.clone(),
                    virtual_uptime_seconds,
                    virtual_realtime_seconds,
                    virtual_memory_kb,
                    virtual_pid,
                    virtual_ppid,
                    virtual_pty_count,
                    fdinfo_identity,
                    random_uuid,
                },
            );
        })?;
        Ok(())
    }

    /// SYS_read system call (MAYHANG).
    pub async fn handle_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            return self
                .read_timer_slack(guest, call.fd(), call.buf(), call.len())
                .await;
        }

        if call.len() == 0 {
            // Zero-count reads only serve to detect errors.
            let res = guest.inject(Syscall::from(call)).await?;
            return Ok(res);
        }

        let needs_procfs_snapshot = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_snapshot())?;
        if needs_procfs_snapshot {
            self.initialize_procfs_snapshot(guest, call).await?;
        }

        let procfs_bytes = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.take_procfs(call.len()))?;
        if let Some(bytes) = procfs_bytes {
            let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
            guest.memory().write_exact(remote_buf, &bytes)?;
            return Ok(bytes.len() as i64);
        }

        let (
            fd_type,
            physically_nonblocking,
            logically_nonblocking,
            resource,
            random_device_offset,
        ) = guest.thread_state_mut().with_detfd(call.fd(), |detfd| {
            (
                detfd.ty(),
                detfd.physically_nonblocking(),
                detfd.is_nonblocking(),
                detfd.resource(),
                detfd.random_device_offset(),
            )
        })?;

        if let Some(resource) = resource {
            let mut request = guest.thread_state().mk_request(resource, Permission::R);
            if should_tag_sabre_internal_pipe_io(
                guest.config().discover_live_file_metadata,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(SABRE_INTERNAL_PIPE_IO_FYI);
            }
            resource_request(guest, request).await;
        }

        let res = match fd_type {
            FdType::Rng => {
                trace!("Read call RNG fd {}, simulating...", call.fd());
                let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
                let n = self.fill_random_device_bytes(
                    guest,
                    remote_buf,
                    call.len(),
                    random_device_offset,
                )?;
                guest.thread_state().with_detfd(call.fd(), |detfd| {
                    detfd.advance_random_device_offset(n);
                })?;
                return Ok(n as i64);
            }
            FdType::Regular => {
                if guest.config().deterministic_io {
                    self.deterministic_read(guest, call).await
                } else {
                    Ok(self.record_or_replay(guest, call).await?)
                }
            }
            FdType::Signalfd | FdType::Eventfd | FdType::Timerfd | FdType::Inotify => {
                trace!(
                    "Possibly blocking read call on notification fd {}, type {:?}",
                    call.fd(),
                    fd_type
                );
                self.execute_nonblockable_fd_syscall(guest, call).await
            }
            FdType::Memfd | FdType::Pidfd | FdType::Userfaultfd | FdType::Epoll => {
                trace!("Read call on unusual fd {}, type {:?}", call.fd(), fd_type);
                Ok(self.record_or_replay(guest, call).await?)
            }

            FdType::Socket | FdType::Pipe => {
                trace!(
                    "Possibly blocking read call on {:?} fd {}",
                    fd_type,
                    call.fd()
                );
                self.execute_nonblockable_fd_syscall(guest, call).await
            }
        };
        resource_release_all(guest).await;
        res
    }

    /// SYS_pread64 system call.
    pub async fn handle_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pread64,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            return self
                .pread_timer_slack(guest, call.fd(), call.buf(), call.len(), call.offset())
                .await;
        }

        if call.len() == 0 {
            // Zero-count reads only serve to detect errors.
            let res = guest.inject(Syscall::from(call)).await?;
            return Ok(res);
        }

        let offset = usize::try_from(call.offset()).map_err(|_| Errno::EINVAL)?;
        let needs_procfs_snapshot = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_snapshot())?;
        if needs_procfs_snapshot {
            let read = syscalls::Read::new()
                .with_fd(call.fd())
                .with_buf(call.buf())
                .with_len(call.len());
            self.initialize_procfs_snapshot(guest, read).await?;
        }

        let procfs_bytes = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.take_procfs_at(offset, call.len()))?;
        if let Some(bytes) = procfs_bytes {
            let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
            guest.memory().write_exact(remote_buf, &bytes)?;
            return Ok(bytes.len() as i64);
        }

        let (fd_type, resource) = guest
            .thread_state_mut()
            .with_detfd(call.fd(), |detfd| (detfd.ty(), detfd.resource()))?;

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::R);
            resource_request(guest, request).await;
        }

        let res = match fd_type {
            FdType::Rng => (|| -> Result<i64, Error> {
                trace!("Pread64 call RNG fd {}, simulating...", call.fd());
                let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
                let n =
                    self.fill_random_device_bytes(guest, remote_buf, call.len(), offset as u64)?;
                Ok(n as i64)
            })(),
            FdType::Regular if guest.config().deterministic_io => {
                self.deterministic_pread64(guest, call).await
            }
            _ => match self.record_or_replay(guest, call).await {
                Ok(value) => Ok(value),
                Err(error) => Err(error.into()),
            },
        };

        resource_release_all(guest).await;
        res
    }

    /// SYS_lseek system call.
    pub async fn handle_lseek<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Lseek,
    ) -> Result<i64, Error> {
        let timer_slack_binding = self.timer_slack_binding(guest, call.fd())?;
        let (fd_type, status_flags, procfs_position, resource) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.status_flags(),
                    detfd.procfs_position(),
                    detfd.resource(),
                )
            })?;
        if fd_type == FdType::Rng {
            return random_device_lseek_result(status_flags, call.whence()).map_err(Into::into);
        }
        if is_inherited_container_output(resource) {
            return Err(Errno::ESPIPE.into());
        }
        if timer_slack_binding.is_some() && status_flags & libc::O_PATH != 0 {
            return Err(Errno::EBADF.into());
        }
        let Some((current, snapshot_len)) = procfs_position else {
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#1044): Regular-file lseek must
            // flow through record_or_replay, exactly as handle_read does for
            // FdType::Regular. Injecting the seek live here is correct for run
            // and --verify (the inner NoopTool just re-injects), but under
            // record/replay the replay descriptor is a virtual placeholder
            // whose kernel position never advances (reads are served from the
            // log, not the file). A live SEEK_CUR then returned Ok(0) on replay
            // versus the recorded offset (e.g. glibc's __tzfile_read rewinds
            // /etc/localtime with lseek(fd, -N, SEEK_CUR)), diverging the
            // guest's control flow and desynchronizing the event stream. Routing
            // through record_or_replay records the offset once and substitutes
            // the recorded value on replay, keeping the two runs identical.
            return Ok(self.record_or_replay(guest, call).await?);
        };

        if let Some(binding) = timer_slack_binding {
            // Linux exposes this file through seq_lseek, which accepts only
            // SEEK_SET and SEEK_CUR. Keep that position entirely in the
            // virtual open-file description even before the first read.
            let requested = i128::from(call.offset());
            let new_offset = match call.whence() {
                Whence::SEEK_SET => requested,
                Whence::SEEK_CUR => current as i128 + requested,
                _ => return Err(Errno::EINVAL.into()),
            };
            let new_offset = usize::try_from(new_offset).map_err(|_| Errno::EINVAL)?;
            let result = i64::try_from(new_offset).map_err(|_| Errno::EOVERFLOW)?;
            // seq_lseek does not call the show callback for a no-op or a reset
            // to zero. Only a traversal to another positive position observes
            // the target task and therefore performs lifetime/access checks.
            if new_offset != 0 && new_offset != current {
                self.require_current_timer_slack_target(guest, binding)
                    .await?;
            }
            guest
                .thread_state()
                .with_detfd(call.fd(), |detfd| detfd.set_procfs_offset(new_offset))?;
            return Ok(result);
        }

        let Some(snapshot_len) = snapshot_len else {
            let offset = guest.inject(Syscall::from(call)).await?;
            let offset = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
            guest
                .thread_state()
                .with_detfd(call.fd(), |detfd| detfd.set_procfs_offset(offset))?;
            return Ok(offset as i64);
        };

        let requested = i128::from(call.offset());
        let new_offset = match call.whence() {
            Whence::SEEK_SET => requested,
            Whence::SEEK_CUR => current as i128 + requested,
            Whence::SEEK_END => snapshot_len as i128 + requested,
            Whence::SEEK_DATA => {
                if requested < 0 || requested >= snapshot_len as i128 {
                    return Err(Errno::ENXIO.into());
                }
                requested
            }
            Whence::SEEK_HOLE => {
                if requested < 0 || requested >= snapshot_len as i128 {
                    return Err(Errno::ENXIO.into());
                }
                snapshot_len as i128
            }
            _ => return Err(Errno::EINVAL.into()),
        };
        let new_offset = usize::try_from(new_offset).map_err(|_| Errno::EINVAL)?;
        let result = i64::try_from(new_offset).map_err(|_| Errno::EOVERFLOW)?;
        guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.set_procfs_offset(new_offset))?;
        Ok(result)
    }

    /// Helper for performing a deterministic read that retries until it gets all its
    /// bytes.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#689): Confirm partial reads take precedence over later errors.
    async fn deterministic_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Read,
    ) -> Result<i64, Error> {
        let mut total_read_bytes = 0;
        let mut remaining_buf = call.len();

        trace!(
            "[detcore/det_io]: Requested read buffer size: {:?}",
            remaining_buf
        );

        loop {
            match guest.inject_with_retry(call).await {
                Ok(res) => {
                    remaining_buf -= res as usize;
                    total_read_bytes += res;

                    trace!(
                        "[detcore/det_io]: Remaining read buffer size: {:?}",
                        remaining_buf
                    );

                    if res == 0 || remaining_buf == 0 {
                        break Ok(total_read_bytes);
                    }

                    // Buf is guaranteed to exist as we already issued a syscall.
                    let old_ptr = call.buf().unwrap().as_raw();
                    call = call
                        .with_len(remaining_buf)
                        .with_buf(AddrMut::<u8>::from_raw(old_ptr + res as usize));
                }
                Err(error) if total_read_bytes > 0 => {
                    trace!("[detcore/det_io]: returning {total_read_bytes} bytes before {error}");
                    break Ok(total_read_bytes);
                }
                Err(error) => break Err(error.into()),
            }
        }
    }

    /// Perform a positional read until the requested buffer is full or EOF is reached.
    async fn deterministic_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Pread64,
    ) -> Result<i64, Error> {
        let mut total_read_bytes = 0;
        let mut remaining_buf = call.len();

        trace!(
            "[detcore/det_io]: Requested pread64 buffer size: {:?}",
            remaining_buf
        );

        loop {
            match guest.inject_with_retry(call).await {
                Ok(res) => {
                    remaining_buf -= res as usize;
                    total_read_bytes += res;

                    trace!(
                        "[detcore/det_io]: Remaining pread64 buffer size: {:?}",
                        remaining_buf
                    );

                    if res == 0 || remaining_buf == 0 {
                        break Ok(total_read_bytes);
                    }

                    let old_ptr = call
                        .buf()
                        .expect("successful pread64 requires a valid guest buffer")
                        .as_raw();
                    let offset = call.offset().checked_add(res).ok_or(Errno::EOVERFLOW)?;
                    call = call
                        .with_len(remaining_buf)
                        .with_buf(AddrMut::<u8>::from_raw(old_ptr + res as usize))
                        .with_offset(offset);
                }
                Err(error) => break Err(error.into()),
            }
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-838): Review regular-file sendfile mediation.
    /// Copy data between tracked regular files or memfds.
    ///
    /// The kernel advances the input offset (or the explicit offset pointer) and
    /// destination offset atomically with the copy. Detcore serializes destination
    /// writes while the strict scheduler orders the stable input read, and routes
    /// the syscall through record/replay so that the result and offset update stay
    /// ordered with other file operations. Socket and pipe destinations can block
    /// and need the nonblocking scheduler path; return ENOSYS for those endpoint
    /// types so libc/application fallbacks use Detcore's existing read/write
    /// handlers instead.
    pub async fn handle_sendfile<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sendfile,
    ) -> Result<i64, Error> {
        let in_type = guest
            .thread_state()
            .with_detfd(call.in_fd(), |detfd| detfd.ty())?;
        let (out_type, out_resource, out_inode) =
            guest.thread_state().with_detfd(call.out_fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.resource(),
                    detfd.stat().map(|stat| stat.inode),
                )
            })?;

        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-973): Refuse sendfile from a procfs input so it
        // cannot bypass the deterministic ProcfsFile snapshot. A procfs fd is
        // classified `FdType::Regular`, so it would otherwise pass the type
        // guard below and copy live kernel bytes straight to the output fd,
        // reintroducing the nondeterminism the mediated read()/write() path
        // sanitizes. Failing closed with ENOSYS makes callers fall back to
        // that mediated path (glibc's sendfile does exactly this).
        let in_is_procfs = guest
            .thread_state()
            .with_detfd(call.in_fd(), |detfd| detfd.procfs_position().is_some())?;
        if in_is_procfs {
            return Err(Errno::ENOSYS.into());
        }

        if !matches!(in_type, FdType::Regular | FdType::Memfd)
            || !matches!(out_type, FdType::Regular | FdType::Memfd)
        {
            return Err(Errno::ENOSYS.into());
        }

        let dettid = guest.thread_state().dettid;
        let mut resources = Resources::new(dettid);
        // `out_inode` is the fd's cached HOST inode, so it must be
        // determinized before naming a resource. It is deliberately left raw
        // for the `touch_file` call below, which takes a `RawInode`.
        let out_resource = match out_resource {
            Some(resource) => Some(resource),
            None => match out_inode {
                Some(raw_ino) => Some(ResourceID::FileContents(
                    determinize_inode(guest, raw_ino).await.0,
                )),
                None => None,
            },
        };
        if let Some(resource) = out_resource {
            resources.insert(resource, Permission::W);
        }
        resources.fyi("sendfile");
        resource_request(guest, resources).await;

        let result = self
            .record_or_replay(guest, call)
            .await
            .map_err(Error::from);
        if guest.config().virtualize_metadata && matches!(&result, Ok(copied) if *copied > 0) {
            let inode = out_inode.expect("virtualized metadata requires stat data for sendfile");
            touch_file(guest, inode).await;
        }
        resource_release_all(guest).await;
        result
    }

    /// SYS_write system call.
    pub async fn handle_write<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Write,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            return self
                .write_timer_slack(guest, call.fd(), call.buf(), call.len())
                .await;
        }

        let (
            fd_type,
            physically_nonblocking,
            logically_nonblocking,
            open_file_id,
            resource,
            raw_ino,
        ) = guest.thread_state().with_detfd(call.fd(), |detfd| {
            (
                detfd.ty(),
                detfd.physically_nonblocking(),
                detfd.is_nonblocking(),
                detfd.open_file_id(),
                detfd.resource(),
                detfd.stat().map(|x| x.inode),
            )
        })?;
        // It doesn't matter much where the linearization point for this mtime bump falls:
        if guest.config().virtualize_metadata {
            let r =
                raw_ino.expect("Expect that when virtualize_metadata, DetFd's stat is populated!");
            touch_file(guest, r).await;
        }

        if let Some(resource) = resource {
            let mut request = guest.thread_state().mk_request(resource, Permission::W);
            if should_tag_sabre_internal_pipe_io(
                guest.config().discover_live_file_metadata,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(SABRE_INTERNAL_PIPE_IO_FYI);
            }
            resource_request(guest, request).await;
        }

        // Only route writes through the nonblockable-fd path when the fd is actually
        // physically nonblocking. Detcore-created pipes are physically nonblocking in every
        // sequential mode, including record/replay, so their logically blocking writes use
        // the completion helper below. A physically blocking fd instead uses the original
        // synchronous path: treating an internal pipe as BlockingExternalIO would assume the
        // writer and reader were independent and could deadlock the scheduler.
        let res = if physically_nonblocking && fd_type == FdType::Pipe && !logically_nonblocking {
            self.execute_blocking_pipe_write(guest, call, open_file_id)
                .await
        } else if physically_nonblocking
            && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd)
        {
            self.execute_nonblockable_fd_syscall(guest, call).await
        } else if guest.config().deterministic_io {
            let mut total_written_bytes = 0;
            let mut remaining_buf = call.len();

            trace!(
                "[detcore/det_io]: Requested write buffer size: {:?}",
                remaining_buf
            );

            loop {
                match self
                    .record_or_replay_preserving_tool_errors(guest, call)
                    .await
                {
                    Ok(res) => {
                        remaining_buf -= res as usize;
                        total_written_bytes += res;

                        trace!(
                            "[detcore/det_io]: Remaining write buffer size: {:?}",
                            remaining_buf
                        );

                        if res == 0 || remaining_buf == 0 {
                            break Ok(total_written_bytes);
                        }

                        // Buf is guaranteed to exist as we already issued a syscall.
                        let old_ptr = call.buf().unwrap().as_raw();
                        call = call
                            .with_len(remaining_buf)
                            .with_buf(Addr::<u8>::from_raw(old_ptr + res as usize));
                    }
                    Err(error) => {
                        break finish_partial_record_or_replay_write(total_written_bytes, error);
                    }
                }
            }
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };

        resource_release_all(guest).await;
        res
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#683): Confirm positional-write ordering and replay semantics.
    /// SYS_pwrite64 system call.
    pub async fn handle_pwrite64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Pwrite64,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            return Err(if call.offset() < 0 {
                Errno::EINVAL.into()
            } else {
                Errno::ESPIPE.into()
            });
        }

        let (resource, raw_ino) = guest.thread_state().with_detfd(call.fd(), |detfd| {
            (detfd.resource(), detfd.stat().map(|stat| stat.inode))
        })?;
        // The fd's cached `DetStat` carries the HOST inode (`DetStat` is built
        // straight from `fstat`/`statx`), so it must be determinized before it
        // can name a guest-visible resource. Passing it through directly used
        // to type-check only because `DetInode` was an alias for `RawInode`.
        let resource = match resource {
            Some(resource) => Some(resource),
            None => match raw_ino {
                Some(raw_ino) => Some(ResourceID::FileContents(
                    determinize_inode(guest, raw_ino).await.0,
                )),
                None => None,
            },
        };

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, request).await;
        }

        let result = if guest.config().deterministic_io {
            let mut total_written = 0_i64;
            let mut remaining = call.len();

            loop {
                match self
                    .record_or_replay_preserving_tool_errors(guest, call)
                    .await
                {
                    Ok(written) => {
                        let Ok(written) = usize::try_from(written) else {
                            break Err(Errno::EIO.into());
                        };
                        let Ok(written_i64) = i64::try_from(written) else {
                            break Err(Errno::EIO.into());
                        };
                        if written > remaining {
                            break Err(Errno::EIO.into());
                        }
                        remaining -= written;
                        let Some(next_total) = total_written.checked_add(written_i64) else {
                            break Err(Errno::EIO.into());
                        };
                        total_written = next_total;

                        if written == 0 || remaining == 0 {
                            break Ok(total_written);
                        }

                        let Some(old_buf) = call.buf() else {
                            break Err(Errno::EFAULT.into());
                        };
                        let Some(next_buf) = old_buf.as_raw().checked_add(written) else {
                            break Err(Errno::EFAULT.into());
                        };
                        let Some(next_offset) = call.offset().checked_add(written_i64) else {
                            break Err(Errno::EFBIG.into());
                        };
                        let Some(next_buf) = Addr::<u8>::from_raw(next_buf) else {
                            break Err(Errno::EFAULT.into());
                        };
                        call = call
                            .with_buf(Some(next_buf))
                            .with_len(remaining)
                            .with_offset(next_offset);
                    }
                    Err(error) => {
                        break finish_partial_record_or_replay_write(total_written, error);
                    }
                }
            }
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };

        if guest.config().virtualize_metadata && matches!(&result, Ok(written) if *written > 0) {
            let inode = raw_ino.expect("virtualized metadata requires stat data for tracked fds");
            touch_file(guest, inode).await;
        }

        resource_release_all(guest).await;
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#547)
    /// SYS_writev system call.
    ///
    /// Preserve the initial writev as one kernel operation so its iovec order remains intact.
    /// Detcore adds open-file resource ordering and nonblocking scheduler integration; a
    /// blocking pipe short write is completed by the helper because Hermit injected O_NONBLOCK.
    pub async fn handle_writev<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Writev,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            self.require_timer_slack_access(guest, call.fd(), true)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), call.len())?;
            return self.writev_timer_slack(guest, call.fd(), iovecs, 0).await;
        }

        let (
            fd_type,
            physically_nonblocking,
            logically_nonblocking,
            open_file_id,
            resource,
            raw_ino,
        ) = guest.thread_state().with_detfd(call.fd(), |detfd| {
            (
                detfd.ty(),
                detfd.physically_nonblocking(),
                detfd.is_nonblocking(),
                detfd.open_file_id(),
                detfd.resource(),
                detfd.stat().map(|x| x.inode),
            )
        })?;

        if let Some(resource) = resource {
            let mut request = guest.thread_state().mk_request(resource, Permission::W);
            if should_tag_sabre_internal_pipe_io(
                guest.config().discover_live_file_metadata,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(SABRE_INTERNAL_PIPE_IO_FYI);
            }
            resource_request(guest, request).await;
        }

        let result = if physically_nonblocking && fd_type == FdType::Pipe && !logically_nonblocking
        {
            self.execute_blocking_pipe_writev(guest, call, open_file_id)
                .await
        } else if physically_nonblocking
            && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd)
        {
            self.execute_nonblockable_fd_syscall(guest, call).await
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };

        if guest.config().virtualize_metadata && matches!(&result, Ok(written) if *written > 0) {
            let inode =
                raw_ino.expect("virtualized metadata requires stat data for every tracked fd");
            touch_file(guest, inode).await;
        }

        resource_release_all(guest).await;
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_readv system call: the vectored form of `read`.
    ///
    /// Mirrors [`Self::handle_writev`] for the read direction. Detcore adds
    /// open-file resource ordering and, for physically nonblocking pipe/socket
    /// fds, the nonblocking scheduler integration; otherwise the vectored read is
    /// recorded/replayed as one kernel operation so its iovec order is preserved.
    pub async fn handle_readv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readv,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            self.require_timer_slack_access(guest, call.fd(), false)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), call.len())?;
            return self
                .readv_timer_slack(guest, call.fd(), iovecs, None, 0)
                .await;
        }

        let is_procfs = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_position().is_some())?;
        if is_procfs {
            return Err(Errno::ENOSYS.into());
        }

        let (fd_type, physically_nonblocking, logically_nonblocking, resource) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.physically_nonblocking(),
                    detfd.is_nonblocking(),
                    detfd.resource(),
                )
            })?;

        if let Some(resource) = resource {
            let mut request = guest.thread_state().mk_request(resource, Permission::R);
            if should_tag_sabre_internal_pipe_io(
                guest.config().discover_live_file_metadata,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(SABRE_INTERNAL_PIPE_IO_FYI);
            }
            resource_request(guest, request).await;
        }

        let res = if physically_nonblocking
            && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd)
        {
            self.execute_nonblockable_fd_syscall(guest, call).await
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        };

        resource_release_all(guest).await;
        res
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_preadv system call: the vectored form of `pread64`.
    ///
    /// Positioned reads target seekable files and do not block, so this mirrors
    /// [`Self::handle_pread64`]'s ordering and records/replays the single kernel
    /// operation.
    pub async fn handle_preadv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Preadv,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            let offset = vectored_offset(call.pos_l(), call.pos_h());
            if offset < 0 {
                return Err(Errno::EINVAL.into());
            }
            self.require_timer_slack_access(guest, call.fd(), false)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), call.iov_len())?;
            return self
                .readv_timer_slack(guest, call.fd(), iovecs, Some(offset), 0)
                .await;
        }

        let is_procfs = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_position().is_some())?;
        if is_procfs {
            return Err(Errno::ENOSYS.into());
        }

        let resource = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.resource())?;

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::R);
            resource_request(guest, request).await;
        }

        let res = self
            .record_or_replay_preserving_tool_errors(guest, call)
            .await;
        resource_release_all(guest).await;
        res
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_preadv2 system call: `preadv` with a trailing per-call flags argument,
    /// which record/replay forwards unchanged.
    pub async fn handle_preadv2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Preadv2,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            let offset = vectored_offset(call.pos_l(), call.pos_h());
            if offset < -1 {
                return Err(Errno::EINVAL.into());
            }
            self.require_timer_slack_access(guest, call.fd(), false)?;
            let count = usize::try_from(call.iov_len()).map_err(|_| Errno::EINVAL)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), count)?;
            return if offset == -1 {
                self.readv_timer_slack(guest, call.fd(), iovecs, None, call.flags())
                    .await
            } else {
                self.readv_timer_slack(guest, call.fd(), iovecs, Some(offset), call.flags())
                    .await
            };
        }

        let is_procfs = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_position().is_some())?;
        if is_procfs {
            return Err(Errno::ENOSYS.into());
        }

        let resource = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.resource())?;

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::R);
            resource_request(guest, request).await;
        }

        let res = self
            .record_or_replay_preserving_tool_errors(guest, call)
            .await;
        resource_release_all(guest).await;
        res
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_pwritev system call: the vectored form of `pwrite64`.
    ///
    /// Positioned writes target seekable files and do not block, so this mirrors
    /// [`Self::handle_pwrite64`]'s ordering, records/replays the single kernel
    /// operation, and bumps the virtual mtime on a successful write.
    pub async fn handle_pwritev<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pwritev,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            let offset = vectored_offset(call.pos_l(), call.pos_h());
            return if offset < 0 {
                Err(Errno::EINVAL.into())
            } else {
                Err(Errno::ESPIPE.into())
            };
        }

        let (resource, raw_ino) = guest.thread_state().with_detfd(call.fd(), |detfd| {
            (detfd.resource(), detfd.stat().map(|stat| stat.inode))
        })?;
        // The fd's cached `DetStat` carries the HOST inode (`DetStat` is built
        // straight from `fstat`/`statx`), so it must be determinized before it
        // can name a guest-visible resource. Passing it through directly used
        // to type-check only because `DetInode` was an alias for `RawInode`.
        let resource = match resource {
            Some(resource) => Some(resource),
            None => match raw_ino {
                Some(raw_ino) => Some(ResourceID::FileContents(
                    determinize_inode(guest, raw_ino).await.0,
                )),
                None => None,
            },
        };

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, request).await;
        }

        let result = self
            .record_or_replay_preserving_tool_errors(guest, call)
            .await;

        if guest.config().virtualize_metadata && matches!(&result, Ok(written) if *written > 0) {
            let inode = raw_ino.expect("virtualized metadata requires stat data for tracked fds");
            touch_file(guest, inode).await;
        }

        resource_release_all(guest).await;
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_pwritev2 system call: `pwritev` with a trailing per-call flags
    /// argument, which record/replay forwards unchanged.
    pub async fn handle_pwritev2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pwritev2,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            let offset = vectored_offset(call.pos_l(), call.pos_h());
            if offset < -1 {
                return Err(Errno::EINVAL.into());
            }
            if offset >= 0 {
                return Err(Errno::ESPIPE.into());
            }
            self.require_timer_slack_access(guest, call.fd(), true)?;
            let count = usize::try_from(call.iov_len()).map_err(|_| Errno::EINVAL)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), count)?;
            return self
                .writev_timer_slack(guest, call.fd(), iovecs, call.flags())
                .await;
        }

        let (resource, raw_ino) = guest.thread_state().with_detfd(call.fd(), |detfd| {
            (detfd.resource(), detfd.stat().map(|stat| stat.inode))
        })?;
        // The fd's cached `DetStat` carries the HOST inode (`DetStat` is built
        // straight from `fstat`/`statx`), so it must be determinized before it
        // can name a guest-visible resource. Passing it through directly used
        // to type-check only because `DetInode` was an alias for `RawInode`.
        let resource = match resource {
            Some(resource) => Some(resource),
            None => match raw_ino {
                Some(raw_ino) => Some(ResourceID::FileContents(
                    determinize_inode(guest, raw_ino).await.0,
                )),
                None => None,
            },
        };

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, request).await;
        }

        let result = self
            .record_or_replay_preserving_tool_errors(guest, call)
            .await;

        if guest.config().virtualize_metadata && matches!(&result, Ok(written) if *written > 0) {
            let inode = raw_ino.expect("virtualized metadata requires stat data for tracked fds");
            touch_file(guest, inode).await;
        }

        resource_release_all(guest).await;
        result
    }

    /// SYS_mmap system call.
    pub async fn handle_mmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Mmap,
    ) -> Result<i64, Error> {
        enum SharedBacking {
            Anonymous,
            File {
                object: SharedMemoryObjectId,
                offset: u64,
            },
        }

        let backing = if call.flags().contains(MapFlags::MAP_SHARED) {
            if call.fd() == -1 {
                Some(SharedBacking::Anonymous)
            } else {
                let offset = u64::try_from(call.offset()).map_err(|_| Errno::EINVAL)?;
                guest
                    .thread_state()
                    .with_detfd(call.fd(), |fd| {
                        let object = fd.stat().map_or_else(
                            || SharedMemoryObjectId::OpenFile {
                                id: fd.open_file_id(),
                            },
                            |stat| SharedMemoryObjectId::File {
                                device: stat.dev,
                                inode: stat.inode,
                            },
                        );
                        SharedBacking::File { object, offset }
                    })
                    .ok()
            }
        } else {
            None
        };
        let len = call.len();
        let result = self.record_or_replay(guest, call).await?;
        let start = usize::try_from(result).expect("a successful mmap must return an address");

        guest.thread_state().unmap_memory(start, len);
        match backing {
            Some(SharedBacking::Anonymous) => {
                guest.thread_state().map_shared_anonymous(start, len);
            }
            Some(SharedBacking::File { object, offset }) => {
                guest
                    .thread_state()
                    .map_shared_object(start, len, object, offset);
            }
            None => {}
        }
        Ok(result)
    }

    /// SYS_munmap system call.
    pub async fn handle_munmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Munmap,
    ) -> Result<i64, Error> {
        let start = call.addr().map(Addr::as_raw).unwrap_or(0);
        let len = call.len();
        let result = self.record_or_replay(guest, call).await?;
        guest.thread_state().unmap_memory(start, len);
        Ok(result)
    }

    /// SYS_mremap system call.
    pub async fn handle_mremap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Mremap,
    ) -> Result<i64, Error> {
        let old_start = call.addr().map(AddrMut::as_raw).unwrap_or(0);
        let old_len = call.old_len();
        let new_len = call.new_len();
        let result = self.record_or_replay(guest, call).await?;
        let new_start =
            usize::try_from(result).expect("a successful mremap must return an address");
        guest
            .thread_state()
            .remap_memory(old_start, old_len, new_start, new_len);
        Ok(result)
    }

    // Determinize stat by doing:
    //   - using virtual inode instead of real inodes. The virtual inodes
    //     increase monolitically and won't be re-used (like ext4)
    //   - use logical modtime which could be used by program like GNU make
    //     to determine file changes
    async fn determinize_stat<G, S>(
        &self,
        guest: &mut G,
        stat: S,
        inode_override: Option<DetInode>,
    ) -> Result<DetStat, Error>
    where
        G: Guest<Self>,
        S: Into<DetStat>,
    {
        let cfg = guest.config().clone();

        let mut stat: DetStat = stat.into();
        let (d_ino, global_mtime) = match inode_override {
            Some(inode) => {
                let nanos = cfg
                    .epoch
                    .timestamp_nanos_opt()
                    .expect("epoch cannot be represented in nanoseconds")
                    as u64;
                (inode, LogicalTime::from_nanos(nanos))
            }
            None => determinize_inode(guest, stat.inode).await,
        };
        stat.inode = d_ino.as_raw(); // Reveal only the deterministic inode.

        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping.
        // The raw st_dev leaks the kernel's host-wide anonymous block-device
        // number for procfs/sysfs/tmpfs mounts, which drifts between runs (and
        // between the two runs of `--verify`). Reveal only a deterministic
        // device id.
        stat.dev = determinize_device(guest, stat.dev).await;

        let epoch_tp = Timespec {
            tv_sec: cfg.epoch.timestamp(),
            tv_nsec: cfg.epoch.timestamp_subsec_nanos() as i64,
        };

        let mtime: Timespec = global_mtime.into();
        stat.atime = epoch_tp;
        stat.ctime = epoch_tp;
        stat.btime = epoch_tp;

        stat.mtime = mtime;

        Ok(stat)
    }

    /// Handles all stat syscalls.
    pub async fn handle_stat_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: StatFamily,
    ) -> Result<i64, Error> {
        if guest.config().virtualize_metadata {
            // NB: let kernel handle error codes, it's not easy to do so without
            // kernel because there're many corner cases. i.e.: even access
            // filepath from tracer may cause tracer to hang under certain fuse
            // filesystem (squashfs_ll).
            guest.inject(Syscall::from(call)).await?;
            let statptr = call.stat().ok_or(Errno::EFAULT)?;
            let inode_override = match call {
                StatFamily::Fstat(call) => guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| {
                        deterministic_stdio_inode_for_resource(call.fd(), detfd.resource())
                    })
                    .ok()
                    .flatten(),
                _ => None,
            };
            let mut memory = guest.memory();
            let stat = memory.read_value(statptr.0)?;
            let stat = self.determinize_stat(guest, stat, inode_override).await?;
            memory.write_value(statptr.0, &stat.into())?;
            Ok(0)
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// statx system call
    pub async fn handle_statx<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Statx,
    ) -> Result<i64, Error> {
        if guest.config().virtualize_metadata {
            // NB: let kernel handle error codes, it's not easy to do so without kernel
            // because there're many corner cases. i.e.: even access filepath from tracer
            // may cause tracer to hang under certain fuse filesystem (squashfs_ll).
            guest.inject(call).await?;
            let statptr = call.statx().ok_or(Errno::EFAULT)?;
            let mut memory = guest.memory();
            let stat = memory.read_value(statptr.0)?;
            let stat = self.determinize_stat(guest, stat, None).await?;
            memory.write_value(statptr.0, &stat.into())?;
            Ok(0)
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// fcntl system call
    pub async fn handle_fcntl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Fcntl,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let o_cloexec = match call.cmd() {
            F_DUPFD_CLOEXEC(_) => OFlag::O_CLOEXEC,
            _ => OFlag::empty(),
        };
        match call.cmd() {
            F_GETFL => {
                let physical_flags = self.record_or_replay(guest, call).await?;
                let logical_nonblocking = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.is_nonblocking())?;
                let nonblocking = i64::from(OFlag::O_NONBLOCK.bits());
                if logical_nonblocking {
                    Ok(physical_flags | nonblocking)
                } else {
                    Ok(physical_flags & !nonblocking)
                }
            }
            F_SETFL(flags) => {
                let fd_type = guest.thread_state().with_detfd(fd, |detfd| detfd.ty())?;
                let force_nonblocking = self.cfg.use_nonblocking_sockets()
                    && !self.cfg.recordreplay_modes
                    && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd);
                let physical_flags = if force_nonblocking {
                    flags | OFlag::O_NONBLOCK.bits()
                } else {
                    flags
                };
                let result = self
                    .record_or_replay(guest, call.with_cmd(F_SETFL(physical_flags)))
                    .await?;
                guest.thread_state().with_detfd(fd, |detfd| {
                    // Record the guest's *logical* status flags (derives logical
                    // nonblocking); when we forced O_NONBLOCK physically without the
                    // guest asking, mark the description physically nonblocking too.
                    detfd.set_status_flags(flags);
                    if force_nonblocking {
                        detfd.set_physically_nonblocking();
                    }
                })?;
                Ok(result)
            }
            F_DUPFD(_) | F_DUPFD_CLOEXEC(_) => {
                let newfd = self.record_or_replay(guest, call).await? as RawFd;
                let replaced = guest.thread_state_mut().dup_fd(fd, newfd, o_cloexec)?;
                if let Some(open_file_id) = replaced {
                    self.release_port_for_open_file(guest, open_file_id).await;
                }
                Ok(newfd as i64)
            }
            F_SETFD(flags) => {
                let result = self.record_or_replay(guest, call).await?;
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_cloexec(flags & libc::FD_CLOEXEC != 0);
                })?;
                Ok(result)
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-2568): Review refusing guest pipe growth.
            // Raising a pipe above the pinned capacity is the ONE syscall that
            // defeats the pin, and whether it succeeds is decided by the host's
            // `/proc/sys/fs/pipe-max-size`. On a default host (1 MiB ceiling)
            // the guest gets a 1048576-byte pipe; on a hardened host (64 KiB)
            // the identical guest gets EPERM and keeps 8192. Same binary, same
            // `--strict`, guest-visible return value and pipe capacity decided
            // by a host sysctl -- a determinism leak by this project's own
            // definition, and it survived because the capacity pin was applied
            // at creation and never defended afterwards.
            //
            // Refuse deterministically instead of asking Linux. EPERM is the
            // errno Linux itself returns when that ceiling binds, so the guest
            // sees a shape it must already handle rather than a novel one, and
            // it is the answer the hardened host would have given.
            //
            // SHRINKING IS DELIBERATELY LEFT ALONE. It is always permitted for
            // an unprivileged process, it is process-local with no host-derived
            // input, and `tests/backend-parity/fixtures/pipe_capacity.c` locks
            // it as a guest-visible contract: that fixture shrinks to one page
            // and requires the value to round-trip. Clamping every
            // `F_SETPIPE_SZ` to the pinned capacity would break that contract
            // while fixing nothing that is actually nondeterministic.
            F_SETPIPE_SZ(requested) if pipe_capacity_request_exceeds_ceiling(requested) => {
                trace!(
                    "[detcore] refusing F_SETPIPE_SZ({}) above the deterministic pipe ceiling {}",
                    requested, DETERMINISTIC_PIPE_CAPACITY_BYTES
                );
                Err(Errno::EPERM.into())
            }
            _ => {
                trace!(
                    "[detcore-finishme]: fcntl unhandled cases: {:?}",
                    call.cmd()
                );
                Ok(self.record_or_replay(guest, call).await?)
            }
        }
    }

    /// ioctl system call
    pub async fn handle_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ioctl,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let (cloexec, nonblocking) = match call.request() {
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1142): Review deterministic SIOCETHTOOL rejection.
            // Ethernet link state belongs to the host network namespace and can change between
            // runs. Match record/replay's established policy instead of exposing that state.
            syscalls::ioctl::Request::SIOCETHTOOL(_) => return Err(Errno::ENODEV.into()),
            syscalls::ioctl::Request::FIOCLEX => (Some(true), None),
            syscalls::ioctl::Request::FIONCLEX => (Some(false), None),
            syscalls::ioctl::Request::FIONBIO(value) => {
                let enabled = guest.memory().read_value(value.ok_or(Errno::EFAULT)?)? != 0;
                (None, Some(enabled))
            }
            _ => (None, None),
        };

        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1013): Review logical FIONBIO handling for forced fds.
        // Detcore already keeps scheduler-managed fds physically nonblocking. Satisfy
        // FIONBIO logically instead of forwarding it: some backends cannot apply the
        // ioctl to their proxied pipe fd, and clearing it would violate the scheduler's
        // nonblockize-and-retry invariant. This mirrors F_SETFL's forced state split.
        if let Some(enabled) = nonblocking {
            let (fd_type, physically_nonblocking) = guest
                .thread_state()
                .with_detfd(fd, |detfd| (detfd.ty(), detfd.physically_nonblocking()))?;
            let force_nonblocking = self.cfg.use_nonblocking_sockets()
                && !self.cfg.recordreplay_modes
                && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd);
            if force_nonblocking && physically_nonblocking {
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_logical_nonblocking(enabled);
                })?;
                return Ok(0);
            }
        }

        let result = self.record_or_replay(guest, call).await?;
        if cloexec.is_some() || nonblocking.is_some() {
            guest.thread_state().with_detfd(fd, |detfd| {
                if let Some(enabled) = cloexec {
                    detfd.set_cloexec(enabled);
                }
                if let Some(enabled) = nonblocking {
                    detfd.set_nonblocking(enabled);
                }
            })?;
        }
        Ok(result)
    }

    /// statfs: report deterministic filesystem statistics.
    ///
    /// The kernel's `statfs` reflects live host state: the free-block counts
    /// (`f_bfree`, `f_bavail`), the free-inode count (`f_ffree`) and the device
    /// id (`f_fsid`) all vary between runs as the underlying host filesystem
    /// fills and drains, which makes a bare passthrough diverge under `--verify`
    /// (e.g. `tar` calls statfs on its target filesystem). The static geometry
    /// of the mount (`f_type`, `f_bsize`, `f_blocks`, `f_namelen`, ...) is
    /// reproducible, so we run the real syscall and then canonicalize only the
    /// volatile fields.
    pub async fn handle_statfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Statfs,
    ) -> Result<i64, Error> {
        let ret = self.record_or_replay(guest, call).await?;
        self.canonicalize_statfs_buf(guest, call.buf())?;
        Ok(ret)
    }

    /// fstatfs: same determinization as [`Self::handle_statfs`], keyed on an fd.
    pub async fn handle_fstatfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Fstatfs,
    ) -> Result<i64, Error> {
        let ret = self.record_or_replay(guest, call).await?;
        self.canonicalize_statfs_buf(guest, call.buf())?;
        Ok(ret)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#1851): Determinized ownership mutation. Emulate the
    // IDENTITY half of the call and let Linux answer the ARGUMENT half.
    /// The `chown` family (`chown`, `fchown`, `fchownat`, `lchown`).
    ///
    /// Detcore presents a fixed virtual-root identity, so the *permission*
    /// answer must be the one a real root gets: success, for any uid. But root
    /// privilege affects only the ownership permission check — it does not
    /// waive pathname, descriptor, or flag errors. A real root's
    /// `chown("/does/not/exist", 0, 0)` still fails with `ENOENT`.
    ///
    /// So this does not fabricate a bare `Ok(0)`. It translates the mutation
    /// into a side-effect-free metadata lookup with the same target-selection
    /// arguments, and reports success only if that lookup succeeds:
    ///
    /// * `F_GETFL` validates `fchown`'s descriptor and distinguishes an
    ///   `O_PATH` descriptor (valid for `fstat`, invalid for `fchown`);
    ///   `newfstatat` performs the corresponding path walk for the three
    ///   pathname variants, preserving `ENOENT`, `ENOTDIR`, `ELOOP`,
    ///   `ENAMETOOLONG`, `EFAULT`, and `EBADF`;
    /// * `fchownat` flags are checked explicitly before the lookup, so an
    ///   unsupported flag still returns `EINVAL` rather than being accepted by
    ///   a metadata syscall with a wider flag vocabulary;
    /// * the ownership assignment itself is not performed, but the *other*
    ///   consequences of a successful chown are, because Linux applies them
    ///   even when ownership does not change. Measured on this host with
    ///   `chown(path, -1, -1)`: mode `06755`, `04755` and `02755` all become
    ///   `0755`; `02644` keeps `S_ISGID` because the file is not
    ///   group-executable; a directory at `06755` keeps both bits; and ctime
    ///   moves in every one of those cases, including the plain `0644` file
    ///   with nothing to clear. Skipping that is a privilege-containment
    ///   regression, not a bookkeeping one: a guest that builds a setuid
    ///   binary and chowns it would see the setuid bit survive under hermit
    ///   and be cleared on the kernel.
    ///
    /// Rather than reimplement that rule, the consequence is delegated to the
    /// kernel by reissuing the *same* call from the same family with the
    /// `(-1, -1)` sentinel, which is precisely the operation whose only effects
    /// are `ATTR_CTIME | ATTR_KILL_SUID | ATTR_KILL_SGID`. Delegation gets the
    /// directory exemption, the group-executable condition on `S_ISGID`, and
    /// symlink handling right for free, and cannot drift from the kernel the
    /// way a transcribed rule would.
    ///
    /// Routed through `record_or_replay` rather than `inject`, so a replay does
    /// not need the guest's filesystem to still exist.
    ///
    /// **Semantic boundary, stated explicitly.** Detcore does not model
    /// per-file ownership, so the success is not observable through a later
    /// `stat`. A guest that chowns to a foreign uid and reads the owner back
    /// sees the unchanged owner — a divergence a single-uid container cannot
    /// avoid, and strictly smaller than the status quo in which the guest
    /// believes it is root and cannot chown at all.
    ///
    /// **Residual, also stated.** This emulates ownership permission and target
    /// validation, not every write-time filesystem policy. In particular a
    /// target on a read-only mount can pass the metadata lookup where a real
    /// chown would return `EROFS`. Path resolution also still requires search
    /// permission on the parent directories, so `EACCES` remains a function of
    /// the host identity under `--no-namespace`. That exposure is shared with
    /// every pass-through filesystem syscall (`open`, `stat`, `chmod`) and is
    /// not introduced here; it is recorded so the boundary is not overstated.
    ///
    /// **Residual introduced by the delegation, stated too.** The sentinel call
    /// needs the same `inode_owner_or_capable` permission the mode change does,
    /// so on a target the guest does not own it returns `EPERM` and that errno
    /// is propagated. Reporting a successful chown while silently failing to
    /// apply the consequence the kernel guarantees would be the same defect in
    /// a narrower form, so this fails closed instead. It is not a regression:
    /// that is exactly the case in which the pass-through implementation also
    /// returned `EPERM`. The case this change exists to fix — a guest chowning
    /// a file it created — is the owning case, and it succeeds.
    ///
    /// The behavioural contract is bracketed end to end by
    /// `hermit-cli/tests/chown_virtual_root_identity.rs`; the unit tests in
    /// `syscall_classification` pin membership only and cannot see this
    /// function's result.
    pub async fn handle_ownership_change_noop<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        // Unreachable while `is_ownership_change_noop_syscall` and the matches
        // below name the same four syscalls. Fail closed if they drift: "not
        // attempted" must never be observationally identical to a validated
        // emulated success.
        if !matches!(
            call,
            Syscall::Chown(_) | Syscall::Fchown(_) | Syscall::Fchownat(_) | Syscall::Lchown(_)
        ) {
            warn!(
                "ownership-change no-op reached with an unexpected syscall {:?}; \
                 refusing unvalidated success",
                call.number()
            );
            return Err(Error::Errno(Errno::ENOSYS));
        }

        // Flag errors precede path resolution in the kernel, so check first.
        if let Syscall::Fchownat(c) = &call {
            let allowed = AtFlags::AT_EMPTY_PATH | AtFlags::AT_SYMLINK_NOFOLLOW;
            if c.flags().bits() & !allowed.bits() != 0 {
                return Err(Error::Errno(Errno::EINVAL));
            }
        }

        // Argument half. `F_GETFL` answers it for `fchown`: it validates the
        // descriptor and distinguishes an `O_PATH` descriptor, which `fstat`
        // accepts and `fchown` rejects. For the three pathname variants a
        // `newfstatat` with the same target-selection arguments performs the
        // corresponding path walk.
        if let Syscall::Fchown(c) = &call {
            let flags = self
                .record_or_replay(
                    guest,
                    syscalls::Fcntl::new().with_fd(c.fd()).with_cmd(F_GETFL),
                )
                .await?;
            if flags & i64::from(OFlag::O_PATH.bits()) != 0 {
                return Err(Error::Errno(Errno::EBADF));
            }
        } else {
            let mut stack = guest.stack().await;
            let statptr: StatPtr = StatPtr(stack.reserve());
            stack.commit()?;

            let validate = match &call {
                Syscall::Chown(c) => Syscall::Newfstatat(
                    syscalls::Newfstatat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(c.path())
                        .with_stat(Some(statptr))
                        .with_flags(AtFlags::empty()),
                ),
                Syscall::Lchown(c) => Syscall::Newfstatat(
                    syscalls::Newfstatat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(c.path())
                        .with_stat(Some(statptr))
                        .with_flags(AtFlags::AT_SYMLINK_NOFOLLOW),
                ),
                Syscall::Fchownat(c) => Syscall::Newfstatat(
                    syscalls::Newfstatat::new()
                        .with_dirfd(c.dirfd())
                        .with_path(c.path())
                        .with_stat(Some(statptr))
                        .with_flags(c.flags()),
                ),
                _ => return Err(Error::Errno(Errno::ENOSYS)),
            };

            // The errno of the side-effect-free validating call is the guest's
            // answer; only an actually executed successful validation becomes the
            // emulated success. Clear the scratch output on both paths.
            let result = self.record_or_replay(guest, validate).await;
            guest
                .memory()
                .write_exact(statptr.0.cast(), &[0; std::mem::size_of::<libc::stat>()])?;
            result?;
        }

        // Metadata half, delegated to the kernel. The identity assignment is
        // deliberately not performed, but everything else a successful chown
        // does is, by reissuing the same call with the `(-1, -1)` sentinel:
        // clear `S_ISUID`, clear `S_ISGID` on a group-executable file, exempt
        // directories, and move ctime unconditionally. Letting Linux apply its
        // own rule keeps it from drifting here.
        const KEEP_ID: libc::uid_t = libc::uid_t::MAX;
        let consequence = match &call {
            Syscall::Fchown(c) => Syscall::Fchown(
                syscalls::Fchown::new()
                    .with_fd(c.fd())
                    .with_owner(KEEP_ID)
                    .with_group(KEEP_ID),
            ),
            Syscall::Chown(c) => Syscall::Chown(
                syscalls::Chown::new()
                    .with_path(c.path())
                    .with_owner(KEEP_ID)
                    .with_group(KEEP_ID),
            ),
            Syscall::Lchown(c) => Syscall::Lchown(
                syscalls::Lchown::new()
                    .with_path(c.path())
                    .with_owner(KEEP_ID)
                    .with_group(KEEP_ID),
            ),
            Syscall::Fchownat(c) => Syscall::Fchownat(
                syscalls::Fchownat::new()
                    .with_dirfd(c.dirfd())
                    .with_path(c.path())
                    .with_owner(KEEP_ID)
                    .with_group(KEEP_ID)
                    .with_flags(c.flags()),
            ),
            _ => return Err(Error::Errno(Errno::ENOSYS)),
        };
        self.record_or_replay(guest, consequence).await?;
        Ok(0)
    }

    /// Overwrite the host-varying fields of a `statfs` result buffer with fixed
    /// values, leaving the static per-mount geometry intact. Shared by statfs
    /// and fstatfs. A null buffer (only possible on an error return, which the
    /// caller has already propagated) is a no-op.
    fn canonicalize_statfs_buf<G: Guest<Self>>(
        &self,
        guest: &mut G,
        buf: Option<AddrMut<libc::statfs>>,
    ) -> Result<(), Error> {
        // Fixed *caps* for the volatile counters. The exact values are
        // arbitrary; they only need to be constant so repeated runs agree. We
        // clamp each free count to the mount's (static) total so we never report
        // the impossible "free > total": a filesystem may be smaller than the
        // cap, and some (e.g. overlayfs) report no inode accounting at all
        // (`f_files == 0`).
        const FREE_BLOCKS_CAP: libc::fsblkcnt_t = 1_000_000;
        const FREE_INODES_CAP: libc::fsfilcnt_t = 500_000;

        if let Some(buf) = buf {
            let mut sf = guest.memory().read_value(buf)?;
            let free_blocks = FREE_BLOCKS_CAP.min(sf.f_blocks);
            sf.f_bfree = free_blocks;
            sf.f_bavail = free_blocks;
            // `f_files == 0` means the filesystem does not track inodes; keep the
            // free count at 0 rather than inventing free inodes on a mount that
            // reports none.
            sf.f_ffree = if sf.f_files == 0 {
                0
            } else {
                FREE_INODES_CAP.min(sf.f_files)
            };
            // f_fsid is a device-dependent filesystem identifier; zero it. An
            // all-zero bit pattern is a valid `fsid_t` (a POD id pair).
            sf.f_fsid = unsafe { std::mem::zeroed() };
            guest.memory().write_value(buf, &sf)?;
        }
        Ok(())
    }

    /// dup system call.
    pub async fn handle_dup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = self.record_or_replay(guest, call).await? as RawFd;
        let replaced = guest
            .thread_state_mut()
            .dup_fd(old_fd, new_fd, OFlag::empty())?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(new_fd as i64)
    }

    /// dup2 system call.
    pub async fn handle_dup2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup2,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = call.newfd();
        let res = self.record_or_replay(guest, call).await?;
        let replaced = guest
            .thread_state_mut()
            .dup_fd(old_fd, new_fd, OFlag::empty())?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(res)
    }

    /// dup3 system call.
    pub async fn handle_dup3<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup3,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = call.newfd();
        let flags = call.flags();
        let res = self.record_or_replay(guest, call).await?;
        let replaced = guest.thread_state_mut().dup_fd(old_fd, new_fd, flags)?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(res)
    }

    /// pipe2 system call.
    pub async fn handle_pipe2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pipe2,
    ) -> Result<i64, Error> {
        // Pipes are unambiguously container-internal: both endpoints are owned by
        // guest processes. Make them physically nonblocking whenever we sequentialize
        // threads -- INCLUDING record/replay modes. This lets a potentially-blocking
        // pipe read follow the deterministic nonblockize-and-retry (InternalIOPolling)
        // path instead of being descheduled as BlockingExternalIO. A pipe reader and
        // its paired writer are NOT independent, so treating an internal pipe as
        // "external blocking IO" (safe to run in the background and rejoin whenever)
        // deadlocks the sequentialized scheduler in R/R (the documented pipe hang). The
        // physical O_NONBLOCK is Detcore-internal and invisible to the guest (F_GETFL is
        // virtualized), and mirrors what `hermit run --strict` already does for pipes.
        let internally_nonblocking = self.cfg.use_nonblocking_sockets();
        let injected = if internally_nonblocking {
            call.with_flags(call.flags() | OFlag::O_NONBLOCK)
        } else {
            call
        };
        // NO PRE-CALL READ OF `pipefd`. This is load-bearing on three backends, not a style
        // choice. `record_or_replay` below returns early on failure, so every guest memory
        // access AFTER it runs only when pipe2 SUCCEEDED -- and a successful pipe2 means the
        // kernel itself wrote two ints there, which proves the address valid. A pre-call
        // snapshot would be the only access to a kernel-UNVALIDATED address, and `LocalMemory`
        // (reverie-dbt, reverie-e9patch, reverie-liteinst) implements reads as an unsafe
        // `copy_nonoverlapping` that always returns `Ok`: a bad pointer is a hardware fault
        // that `Result::ok` cannot catch, so the guest would die with SIGSEGV before Linux
        // could report EFAULT. Letting the kernel touch `pipefd` first is also what keeps its
        // argument-validation precedence intact -- flags are checked before the pointer, so a
        // bad pointer with bad flags is EINVAL and with good flags EFAULT.
        // A C guest asserting that precedence directly is being added separately.
        let res = self.record_or_replay(guest, injected).await?;
        let memory = guest.memory();

        if let Some(pipefd) = call.pipefd() {
            let fds: [i32; 2] = memory.read_value(pipefd)?;
            if internally_nonblocking {
                let capacity_result = guest
                    .inject(
                        syscalls::Fcntl::new()
                            .with_fd(fds[0])
                            .with_cmd(F_SETPIPE_SZ(DETERMINISTIC_PIPE_CAPACITY_BYTES)),
                    )
                    .await;
                if let Some(failure) = pipe_capacity_failure(fds, capacity_result) {
                    // Release the descriptors Linux already created, THEN stop the run.
                    //
                    // Returning `Err` here -- what this code did before -- unwinds without
                    // closing them. `pipe2` has already succeeded, so both descriptors are
                    // live in the guest and are not yet registered with `add_fd`, which means
                    // Detcore's own bookkeeping never learns they exist. Closing them is the
                    // only way the guest's descriptor table matches Detcore's model.
                    //
                    // We do NOT fabricate a `pipe2` errno. Linux leaves `pipefd` untouched
                    // when `pipe2` fails, so inventing a failure would oblige us to restore
                    // the caller's buffer, which needs the pre-call snapshot the comment above
                    // explains we must never take. And returning success with an unpinned pipe
                    // silently restores exactly the host-dependent capacity this path exists
                    // to remove. An unpinnable pipe means determinism is unavailable for this
                    // run, so fail closed and loudly rather than quietly.
                    //
                    // Defensive, not expected: pinning on a freshly created EMPTY pipe is a
                    // shrink or a no-op. EBUSY needs buffered data and EPERM needs to exceed
                    // `pipe-max-size`; neither can hold here.
                    for close in failure.close_syscalls() {
                        let _ = guest.inject(close).await;
                    }
                    error!(
                        "[detcore] cannot pin scheduler-managed pipe to {} bytes (fds {:?}): {}. \
                         Determinism is unavailable for this run.",
                        DETERMINISTIC_PIPE_CAPACITY_BYTES, failure.created_fds, failure.error,
                    );
                    // Fail-closed policy: determinism is unavailable for this run.
                    unrecoverable_shutdown(guest, detcore_model::HERMIT_POLICY_REFUSAL_EXIT).await;
                }
            }
            self.add_fd(guest, fds[0], call.flags(), FdType::Pipe)
                .await?;
            self.add_fd(guest, fds[1], call.flags(), FdType::Pipe)
                .await?;
            if internally_nonblocking {
                self.maybe_set_nonblocking_fd(guest, fds[0]);
                self.maybe_set_nonblocking_fd(guest, fds[1]);
            }
        }

        Ok(res)
    }

    /// utime syscall: update access/modification time on a file
    pub async fn handle_utime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utime,
    ) -> Result<i64, Errno> {
        let mut stack = guest.stack().await;
        let mut memory = guest.memory();
        let tp: AddrMut<[Timespec; 2]> = stack.reserve();

        let tp_val = match call.times() {
            None => {
                let now: Timespec = thread_observe_time(guest).await.into();
                [now, now]
            }
            Some(times) => {
                let utimptr = times;
                let utimbuf = memory.read_value(utimptr)?;
                [
                    Timespec {
                        tv_sec: utimbuf.actime,
                        tv_nsec: 0,
                    },
                    Timespec {
                        tv_sec: utimbuf.modtime,
                        tv_nsec: 0,
                    },
                ]
            }
        };

        memory.write_value(tp, &tp_val)?;
        stack.commit()?;

        let utimensat = syscalls::Utimensat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(call.path())
            .with_times(Some(tp.into()));

        self.handle_utimensat(guest, utimensat).await
    }

    /// utimes syscall
    pub async fn handle_utimes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimes,
    ) -> Result<i64, Errno> {
        let mut memory = guest.memory();

        let tp: AddrMut<[Timespec; 2]> = match call.times() {
            None => {
                let now: Timespec = thread_observe_time(guest).await.into();
                let mut stack = guest.stack().await;
                let tp: AddrMut<[Timespec; 2]> = stack.reserve();
                memory.write_value(tp, &[now, now])?;
                stack.commit()?;
                tp
            }
            Some(times) => {
                // Convert the timeval array to a timespec array.
                let tvs = memory.read_value(times)?;
                let tp: Addr<[Timespec; 2]> = times.cast();

                // Safety: The address could point to read-only memory and the
                // write below could fail.
                let tp = unsafe { tp.into_mut() };

                memory.write_value(tp, &[tvs[0].into(), tvs[1].into()])?;
                tp
            }
        };

        let utimensat = syscalls::Utimensat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(call.filename())
            .with_times(Some(tp.into()));

        self.handle_utimensat(guest, utimensat).await
    }

    /// ustimensat syscall
    pub async fn handle_utimensat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimensat,
    ) -> Result<i64, Errno> {
        self.record_or_replay(guest, call).await
    }

    /// socket system call.
    pub async fn handle_socket<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socket,
    ) -> Result<i64, Error> {
        // The socket syscall itself is not blocking, but we must decide whether to make the socket
        // returned physically nonblocking.
        if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            // Allow possibly blocking syscall in record mode
            let fd = self.record_or_replay(guest, call).await? as RawFd;
            self.add_fd(
                guest,
                fd,
                OFlag::from_bits_truncate(call.r#type()),
                FdType::Socket,
            )
            .await?;
            self.mark_sock_diag_fd(guest, fd, &call);
            Ok(fd as i64)
        } else {
            // Under run mode, force all sockets to be registered to be nonblocking in the OS:
            let call2 = if self.cfg.use_nonblocking_sockets() {
                call.with_type(call.r#type() | libc::SOCK_NONBLOCK)
            } else {
                call
            };
            let fd = self.record_or_replay(guest, call2).await? as RawFd; // Cannot hang.
            self.add_fd(
                guest,
                fd,
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;
            self.maybe_set_nonblocking_fd(guest, fd);
            self.mark_sock_diag_fd(guest, fd, &call);

            Ok(fd as i64)
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1064)
    /// Flag `AF_NETLINK`/`NETLINK_SOCK_DIAG` sockets so `handle_recvmsg`
    /// determinizes the socket inode numbers carried by their binary dump
    /// replies (see `crate::sock_diag`). Best-effort: if the descriptor lookup
    /// fails the reply is simply left unsanitized.
    fn mark_sock_diag_fd<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd, call: &syscalls::Socket) {
        if call.family() != libc::AF_NETLINK {
            return;
        }
        if call.protocol() == libc::NETLINK_SOCK_DIAG {
            let _ = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.set_sock_diag());
        }
        // TODO-HUMAN-REVIEW(PR-2478)
        // NETLINK_ROUTE link dumps carry live interface counters. They were
        // invisible until IO-buffer hashing went on by default, because the
        // reply's LENGTH and return value are identical between runs and only
        // the payload bytes move.
        if call.protocol() == libc::NETLINK_ROUTE {
            let _ = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.set_netlink_route());
        }
    }

    /// socketpair system call.
    pub async fn handle_socketpair<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socketpair,
    ) -> Result<i64, Error> {
        let call2 = if self.cfg.sequentialize_threads && !self.cfg.debug_externalize_sockets {
            call.with_type(call.r#type() | libc::SOCK_NONBLOCK)
        } else {
            call
        };
        let res = self.record_or_replay(guest, call2).await?;
        if let Some(usockvec) = call.usockvec() {
            let memory = guest.memory();
            let fds: [i32; 2] = memory.read_value(usockvec)?;

            // Logical flags are as requested:
            self.add_fd(
                guest,
                fds[0],
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;
            self.add_fd(
                guest,
                fds[1],
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;

            self.maybe_set_nonblocking_fd(guest, fds[0]);
            self.maybe_set_nonblocking_fd(guest, fds[1]);
        }
        Ok(res)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Apply a socket option to an already tracked socket. Record mode captures
    /// the result; replay re-applies a successful option before later socket I/O,
    /// which remains mediated by Detcore's nonblocking scheduler paths.
    pub async fn handle_setsockopt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setsockopt,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Transition an already tracked socket into listening state.
    pub async fn handle_listen<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Listen,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Return the local address of a tracked socket.
    pub async fn handle_getsockname<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getsockname,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Return the peer address of a tracked socket.
    pub async fn handle_getpeername<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getpeername,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Return an option value from a tracked socket. Hermit only promises normal
    /// run determinism for isolated guest networking; record/replay captures the
    /// result when external socket state is part of the recording boundary.
    pub async fn handle_getsockopt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getsockopt,
    ) -> Result<i64, Error> {
        // TODO-HUMAN-REVIEW(PR-894): Review deterministic network-namespace identity.
        let requested_length =
            if call.level() == libc::SOL_SOCKET && call.optname() == libc::SO_NETNS_COOKIE {
                let fd_type = guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| detfd.ty())?;
                if fd_type == FdType::Socket {
                    call.optlen()
                        .map(|length| guest.memory().read_value(length))
                        .transpose()?
                } else {
                    None
                }
            } else {
                None
            };

        // TODO-HUMAN-REVIEW(PR-886): Review deterministic SO_COOKIE identities.
        let deterministic_cookie =
            if call.level() == libc::SOL_SOCKET && call.optname() == libc::SO_COOKIE {
                let requested_length = call
                    .optlen()
                    .map(|length| guest.memory().read_value(length))
                    .transpose()?;
                let open_file_id = guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| detfd.open_file_id())?;
                Some((open_file_id.deterministic_socket_cookie(), requested_length))
            } else {
                None
            };

        let result = self.record_or_replay(guest, call).await?;

        // TODO-HUMAN-REVIEW(PR-898): Hermit exposes one virtual CPU, so do not
        // leak the host CPU that processed a socket's most recent packet.
        if result == 0
            && call.level() == libc::SOL_SOCKET
            && call.optname() == libc::SO_INCOMING_CPU
            && let (Some(optval), Some(optlen)) = (call.optval(), call.optlen())
        {
            let returned_len: libc::socklen_t = guest.memory().read_value(optlen)?;
            let zero_cpu = 0_i32.to_ne_bytes();
            let returned_len = (returned_len as usize).min(zero_cpu.len());
            guest
                .memory()
                .write_exact(optval.cast::<u8>(), &zero_cpu[..returned_len])?;
        }
        if result == 0
            && call.level() == libc::IPPROTO_TCP
            && call.optname() == libc::TCP_INFO
            && let (Some(optval), Some(optlen)) = (call.optval(), call.optlen())
        {
            let returned_len: libc::socklen_t = guest.memory().read_value(optlen)?;
            let mut info = vec![0; returned_len as usize];
            let optval = optval.cast::<u8>();
            guest.memory().read_exact(optval, info.as_mut_slice())?;
            canonicalize_tcp_info(&mut info);
            guest.memory().write_exact(optval, info.as_slice())?;
        }
        if let Some(requested_length) = requested_length
            && let Some(value) = call.optval()
        {
            let bytes = DETERMINISTIC_NETNS_COOKIE.to_ne_bytes();
            let write_length = (requested_length as usize).min(bytes.len());
            guest
                .memory()
                .write_exact(value.cast(), &bytes[..write_length])?;
        }
        if let Some((cookie, Some(requested_length))) = deterministic_cookie
            && let Some(value) = call.optval()
        {
            let bytes = cookie.to_ne_bytes();
            let write_length = (requested_length as usize).min(bytes.len());
            guest
                .memory()
                .write_exact(value.cast(), &bytes[..write_length])?;
        }
        Ok(result)
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#818)
    /// Half-close the read and/or write direction of an already tracked socket.
    /// shutdown never blocks and returns no data; its effect is deterministic
    /// given the container's socket state, so it forwards via record_or_replay
    /// exactly like the rest of the socket family (KVM ratchet round 12).
    pub async fn handle_shutdown<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Shutdown,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// bind system call.
    pub async fn handle_bind<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Bind,
    ) -> Result<i64, Error> {
        // WIP!
        if guest.config().sched_heuristic == SchedHeuristic::ConnectBind {
            trace!("Scheduling heuristic: reprioritizing bind");
            let resource = ResourceID::PriorityChangePoint(
                LAST_PRIORITY,
                guest.thread_state().thread_logical_time.as_nanos(),
                guest.thread_state().committed_clock_value,
                Vec::new(),
            );
            let req = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, req).await;
        }
        let addr = call.umyaddr().ok_or(Errno::EFAULT)?;
        let sock_fd = call.fd();
        let open_file_id = guest
            .thread_state()
            .with_detfd(sock_fd, |detfd| detfd.open_file_id())?;

        let sockaddr_family = guest.memory().read_value(addr.cast::<u16>())?;
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-872): Review deterministic AF_UNIX autobind identity.
        if sockaddr_family == libc::AF_UNIX as u16
            && call.addrlen() == std::mem::offset_of!(libc::sockaddr_un, sun_path) as i32
        {
            let resp = send_and_update_time(guest, GlobalRequest::RequestPort(open_file_id)).await;
            let port = match resp.1 {
                GlobalResponse::RequestPort(port) => port,
                GlobalResponse::PortFull => {
                    return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                }
                _ => unreachable!(),
            };

            let mut stack = guest.stack().await;
            let autobind_addr: AddrMut<libc::sockaddr_un> = stack.reserve();
            let _stack_guard = stack.commit()?;
            guest
                .memory()
                .write_value(autobind_addr, &unix_autobind_address(port))?;
            let deterministic_bind = call
                .with_umyaddr(Some(autobind_addr.cast()))
                .with_addrlen(unix_autobind_addrlen());
            return Ok(self.record_or_replay(guest, deterministic_bind).await?);
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-880): Review deterministic Netlink autobind identities.
        } else if sockaddr_family == libc::AF_NETLINK as u16
            && call.addrlen() >= std::mem::size_of::<libc::sockaddr_nl>() as i32
        {
            let mut sockaddr_nl: libc::sockaddr_nl = guest
                .memory()
                .read_value(addr.cast::<libc::sockaddr_nl>())?;
            if sockaddr_nl.nl_pid == 0 {
                let resp =
                    send_and_update_time(guest, GlobalRequest::RequestPort(open_file_id)).await;
                match resp.1 {
                    GlobalResponse::RequestPort(port) => {
                        sockaddr_nl.nl_pid = DETERMINISTIC_NETLINK_PORT_ID_BASE | u32::from(port);
                        let mut stack = guest.stack().await;
                        let deterministic_addr: AddrMut<libc::sockaddr_nl> = stack.reserve();
                        let _stack_guard = stack.commit()?;
                        guest
                            .memory()
                            .write_value(deterministic_addr, &sockaddr_nl)?;
                        let deterministic_bind = call.with_umyaddr(Some(deterministic_addr.cast()));
                        return Ok(self.record_or_replay(guest, deterministic_bind).await?);
                    }
                    GlobalResponse::PortFull => {
                        return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                    }
                    _ => unreachable!(),
                }
            }
        } else if sockaddr_family == libc::AF_INET as u16 {
            // For IPv4
            let mut sockaddr_in: libc::sockaddr_in = guest
                .memory()
                .read_value(addr.cast::<libc::sockaddr_in>())?;

            let port = sockaddr_in.sin_port.to_be();
            let ipaddr = Ipv4Addr::from(sockaddr_in.sin_addr.s_addr);
            if port != 0 {
                if guest.config().warn_non_zero_binds {
                    warn!(
                        "Analyze Networking: Non-zero port detected: {:?}:{:?}",
                        ipaddr, port
                    );
                }
                // Send RPC to make sure already used ports are not used.
                let resp =
                    send_and_update_time(guest, GlobalRequest::AddUsedPort(port, open_file_id))
                        .await;
                match resp.1 {
                    GlobalResponse::AddUsedPort => {
                        trace!("Added to used port {}", port);
                    }
                    _ => unreachable!(),
                }
            } else {
                // Request a determinzed port
                let resp =
                    send_and_update_time(guest, GlobalRequest::RequestPort(open_file_id)).await;
                match resp.1 {
                    GlobalResponse::RequestPort(port_assigned) => {
                        sockaddr_in.sin_port = port_assigned.to_be();
                        guest
                            .memory()
                            .write_value(addr.cast::<libc::sockaddr_in>(), &sockaddr_in)?;
                    }
                    GlobalResponse::PortFull => {
                        return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                    }
                    _ => unreachable!(),
                }
            }
        } else if sockaddr_family == libc::AF_INET6 as u16 {
            // For IPv6
            let mut sockfaddr_in: libc::sockaddr_in6 = guest
                .memory()
                .read_value(addr.cast::<libc::sockaddr_in6>())?;
            let port = sockfaddr_in.sin6_port.to_be();
            let ipaddr = Ipv6Addr::from(sockfaddr_in.sin6_addr.s6_addr);
            if port != 0 {
                if guest.config().warn_non_zero_binds {
                    warn!(
                        "Analyze Networking: Non-zero port detected: {:?}:{:?}",
                        ipaddr, port
                    );
                }
                let resp =
                    send_and_update_time(guest, GlobalRequest::AddUsedPort(port, open_file_id))
                        .await;
                match resp.1 {
                    GlobalResponse::AddUsedPort => {
                        trace!("Added to used port {}", port);
                    }
                    _ => unreachable!(),
                }
            } else {
                let resp =
                    send_and_update_time(guest, GlobalRequest::RequestPort(open_file_id)).await;
                match resp.1 {
                    GlobalResponse::RequestPort(port_assigned) => {
                        sockfaddr_in.sin6_port = port_assigned.to_be();
                        guest
                            .memory()
                            .write_value(addr.cast::<libc::sockaddr_in6>(), &sockfaddr_in)?;
                        trace!("Port assigned {}", port_assigned)
                    }
                    GlobalResponse::PortFull => {
                        return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                    }
                    _ => unreachable!(),
                }
            }
        }
        let res = self.record_or_replay(guest, call).await?;

        Ok(res)
    }

    /// Create and register an event notification counter.
    ///
    /// Determinism: strict execution serializes creation, so the initial counter, guest-visible
    /// flags, and descriptor number depend only on syscall arguments and the reconstructed file
    /// table. Any internally added nonblocking flag remains hidden from the guest.
    pub async fn handle_eventfd2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Eventfd2,
    ) -> Result<i64, Error> {
        let internally_nonblocking =
            self.cfg.use_nonblocking_sockets() && !self.cfg.recordreplay_modes;
        let injected = if internally_nonblocking {
            call.with_flags(call.flags() | syscalls::EfdFlags::EFD_NONBLOCK)
        } else {
            call
        };
        let fd = self.record_or_replay(guest, injected).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::EFD_CLOEXEC | libc::EFD_NONBLOCK),
            ),
            FdType::Eventfd,
        )
        .await?;
        if internally_nonblocking {
            self.maybe_set_nonblocking_fd(guest, fd);
        }
        Ok(fd as i64)
    }

    /// signalfd4 system call.
    pub async fn handle_signalfd4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Signalfd4,
    ) -> Result<i64, Error> {
        let signalfd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            signalfd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::SFD_CLOEXEC | libc::SFD_NONBLOCK),
            ),
            FdType::Signalfd,
        )
        .await?;
        Ok(signalfd as i64)
    }

    /// Create and register a timer notification descriptor.
    ///
    /// Determinism: strict execution serializes creation, which exposes only kernel validation,
    /// guest-visible flags, and a descriptor number; this operation does not read the clock.
    pub async fn handle_timerfd_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdCreate,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::TFD_CLOEXEC | libc::TFD_NONBLOCK),
            ),
            FdType::Timerfd,
        )
        .await?;
        Ok(fd as i64)
    }

    /// Serialize a notification descriptor control operation.
    async fn notification_fd_control<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await;
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// timerfd_settime system call.
    pub async fn handle_timerfd_settime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdSettime,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// timerfd_gettime system call.
    pub async fn handle_timerfd_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdGettime,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// inotify_init1 system call.
    pub async fn handle_inotify_init1<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyInit1,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.flags().bits() & (libc::IN_CLOEXEC | libc::IN_NONBLOCK)),
            FdType::Inotify,
        )
        .await?;
        Ok(fd as i64)
    }

    /// inotify_add_watch system call.
    pub async fn handle_inotify_add_watch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyAddWatch,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// inotify_rm_watch system call.
    pub async fn handle_inotify_rm_watch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyRmWatch,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// memfd_create system call.
    pub async fn handle_memfd_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::MemfdCreate,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate((call.flags() & libc::MFD_CLOEXEC) as i32),
            FdType::Memfd,
        )
        .await?;
        Ok(fd as i64)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-862): pidfd creation and Detcore FD registration.
    /// Create a pidfd through record/replay and synchronize the descriptor with
    /// Detcore's metadata before fcntl, poll, close, or waitid can observe it.
    pub async fn handle_pidfd_open<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::PidfdOpen,
    ) -> Result<i64, Error> {
        let allowed_flags = libc::O_NONBLOCK as u32;
        if call.flags() & !allowed_flags != 0 {
            return Err(Errno::EINVAL.into());
        }

        let fd = self.record_or_replay(guest, call).await? as RawFd;
        let flags = OFlag::O_CLOEXEC | OFlag::from_bits_truncate(call.flags() as libc::c_int);
        self.add_fd(guest, fd, flags, FdType::Pidfd).await?;
        let target = DetPid::from_raw(call.pid() as i32);
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.set_pidfd_target(target))?;
        Ok(fd as i64)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1175): pidfd_send_signal(2) determinization.
    /// Deliver a signal to the process referred to by a pidfd.
    ///
    /// `pidfd_send_signal(pidfd, sig, info, flags)` names its target by an open
    /// kernel descriptor rather than a numeric PID. Unlike `kill(2)`, there is
    /// therefore no host-PID/virtual-PID ambiguity for Detcore to resolve: the
    /// pidfd was bound to one specific process at `pidfd_open` time. Signal
    /// generation runs inside this thread's serialized scheduler turn, exactly
    /// like `tgkill`/`tkill`/`rt_tgsigqueueinfo` (which also just forward through
    /// record/replay), so forwarding the kernel call is deterministic by
    /// construction. This handler adds deterministic argument validation ahead of
    /// the forward: a descriptor that Detcore does not model as a pidfd fails
    /// closed with `EBADF`, and the flags field the current kernel reserves is
    /// required to be zero (`EINVAL` otherwise), so the guest-visible errno is
    /// fixed and host-independent.
    ///
    /// `call` is the raw `Syscall::Other`; `pidfd`/`flags` are pre-extracted from
    /// its arguments by the dispatcher.
    pub async fn handle_pidfd_send_signal<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        pidfd: RawFd,
        flags: u32,
    ) -> Result<i64, Error> {
        // The kernel currently reserves `flags`; a nonzero value is EINVAL.
        if flags != 0 {
            return Err(Errno::EINVAL.into());
        }
        // Fail closed unless Detcore models this descriptor as a pidfd. This also
        // yields a deterministic EBADF for an unknown/closed descriptor.
        let is_pidfd = guest
            .thread_state()
            .with_detfd(pidfd, |detfd| matches!(detfd.ty(), FdType::Pidfd))?;
        if !is_pidfd {
            return Err(Errno::EBADF.into());
        }
        Ok(self.record_or_replay(guest, call).await?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1175): pidfd_getfd(2) determinization and the
    // FdType of the duplicated descriptor.
    /// Duplicate a descriptor from the process referred to by a pidfd.
    ///
    /// `pidfd_getfd(pidfd, targetfd, flags)` returns a fresh descriptor in the
    /// caller that aliases `targetfd` in the target process. The source pidfd
    /// names one specific process fixed at `pidfd_open` time, the returned
    /// descriptor number is chosen through record/replay (so it is stable across
    /// runs), and a successful modeled operation executes inside this thread's
    /// serialized turn, so the result is deterministic. For zero flags, Detcore
    /// fails closed with `EBADF` unless it models the descriptor as a pidfd.
    /// Linux checks the kernel-reserved `flags` first, however, so a nonzero
    /// value takes the raw record/replay path and preserves the kernel's exact
    /// `EINVAL` across valid and invalid descriptor combinations.
    ///
    /// The modeled path is narrower than "same process": the caller must be the
    /// thread-group leader named by the pidfd. `CLONE_THREAD` does not imply
    /// `CLONE_FILES`, so a nonleader can share the target's TGID while using a
    /// different descriptor table. Requiring `target == getpid() == gettid()`
    /// proves that `targetfd` is resolved in the caller's exact table. The
    /// returned descriptor is then modeled as a real alias of the source open
    /// file description. Broader support needs a cross-task OFD channel that
    /// Detcore does not have today, so every other target is refused with
    /// `EOPNOTSUPP`. Failed calls leave descriptor state unchanged.
    pub async fn handle_pidfd_getfd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        pidfd: RawFd,
        targetfd: RawFd,
        flags: u32,
    ) -> Result<i64, Error> {
        if flags != 0 {
            return match self.record_or_replay(guest, call).await {
                Err(error) => Err(error.into()),
                Ok(fd) => {
                    let fd = fd as RawFd;
                    let close_result = guest.inject(syscalls::Close::new().with_fd(fd)).await;
                    Err(Error::Tool(anyhow::anyhow!(
                        "pidfd_getfd unexpectedly accepted reserved flags and returned fd {fd}; cleanup close result: {close_result:?}"
                    )))
                }
            };
        }

        if !guest.config().sequentialize_threads {
            return self
                .refuse_unserviceable_operation(guest, Sysno::pidfd_getfd, Errno::EOPNOTSUPP)
                .await;
        }

        let current_tgid = DetPid::from_raw(guest.inject(syscalls::Getpid::new()).await? as i32);
        let current_tid = DetTid::from_raw(guest.inject(syscalls::Gettid::new()).await? as i32);
        let source = guest.thread_state().capture_pidfd_getfd_source(
            pidfd,
            targetfd,
            current_tgid,
            current_tid,
        )?;
        let fd = match self.record_or_replay(guest, call).await {
            Ok(fd) => fd as RawFd,
            Err(error) => {
                if let Some(open_file_id) = guest.thread_state().abandon_captured_fd(source) {
                    self.release_port_for_open_file(guest, open_file_id).await;
                }
                return Err(error.into());
            }
        };
        // pidfd_getfd always sets FD_CLOEXEC on the returned descriptor.
        let replaced = match guest.thread_state_mut().install_captured_fd(
            source,
            fd,
            OFlag::O_CLOEXEC,
        ) {
            Ok(replaced) => replaced,
            Err(error @ CapturedDetFdInstallError { .. }) => {
                let expected_files_id = error.expected_files_id;
                let actual_files_id = error.actual_files_id;
                let cleanup = error.into_cleanup();
                let close_result = guest
                    .inject(syscalls::Close::new().with_fd(cleanup.close_fd))
                    .await;
                if let Some(open_file_id) = cleanup.release_open_file {
                    self.release_port_for_open_file(guest, open_file_id).await;
                }
                if let Err(close_error) = close_result {
                    return Err(Error::Tool(anyhow::anyhow!(
                        "pidfd_getfd returned fd {fd}, but its captured source table changed from {expected_files_id:?} to {actual_files_id:?}; cleanup close failed with {close_error}"
                    )));
                }
                warn!(
                    "pidfd_getfd returned fd {fd}, but its captured source table changed from {expected_files_id:?} to {actual_files_id:?}; closed the result and refusing with EOPNOTSUPP"
                );
                return Err(Errno::EOPNOTSUPP.into());
            }
        };
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(fd as i64)
    }

    /// userfaultfd system call.
    pub async fn handle_userfaultfd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Userfaultfd,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.flags()),
            FdType::Userfaultfd,
        )
        .await?;
        Ok(fd as i64)
    }

    /// accept4 system call (MAYHANG).
    ///
    /// Category: External OR Internal IO
    /// ---------------------------------
    /// When do we know?  We only know if an accept4 did an extra-container IO AFTER it returns.
    /// I.e. we could accept a connection from another endpoint in the container, or from the outside,
    /// and we don't know which at the point where `accept4` is called.
    pub async fn handle_accept4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Accept4,
    ) -> Result<i64, Error> {
        // This option applies both to the socket we're doing the accept call on, and the connection
        // that we return. We don't have any smart detection yet to separate internal/external, so
        // applies to everything.
        let call2 = if self.cfg.use_nonblocking_sockets() {
            // Let the socket returned from accept4 be physically nonblocking:
            call.with_flags(call.flags() | SockFlag::SOCK_NONBLOCK)
        } else {
            call
        };
        // This will do blocking/polling as appropriate based on the fd status:
        let fd = self.execute_nonblockable_fd_syscall(guest, call2).await? as RawFd;

        self.add_fd(
            guest,
            fd,
            // This will specify whether the socket returned is logically non-blocking:
            oflag_from_sock_bits(call.flags().bits()),
            FdType::Socket,
        )
        .await?;

        self.maybe_set_nonblocking_fd(guest, fd);

        Ok(fd as i64)
    }

    /// getdents system call.
    pub async fn handle_getdents<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getdents,
    ) -> Result<i64, Error> {
        if !guest.config().virtualize_metadata {
            return Ok(self.record_or_replay(guest, call).await?);
        }

        let dirent = call.dirent().ok_or(Errno::EFAULT)?;

        let nb = self.record_or_replay(guest, call).await?;
        if nb == 0 {
            return Ok(0);
        }

        let mut dents_bytes = vec![0; nb as usize];
        dents_bytes.reserve_exact(128);

        guest
            .memory()
            .read_exact(dirent.cast(), dents_bytes.as_mut_slice())?;

        let mut dents = unsafe { deserialize_dirents(&dents_bytes) };
        dents.sort();
        for dent in &mut dents {
            let (d_ino, _) = determinize_inode(guest, dent.ino).await;
            dent.ino = d_ino.as_raw();
        }

        let mut dents_bytes = vec![0; dents_bytes.len()];
        let _ = unsafe { serialize_dirents(&dents, &mut dents_bytes) };

        guest
            .memory()
            .write_exact(dirent.cast(), dents_bytes.as_slice())?;
        Ok(nb)
    }

    /// getdents64 system call.
    pub async fn handle_getdents64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getdents64,
    ) -> Result<i64, Error> {
        if !guest.config().virtualize_metadata {
            return Ok(self.record_or_replay(guest, call).await?);
        }

        let dirent = call.dirent().ok_or(Errno::EFAULT)?;

        let nb = self.record_or_replay(guest, call).await?;
        if nb == 0 {
            return Ok(0);
        }

        let mut dents_bytes = vec![0; nb as usize];
        dents_bytes.reserve_exact(128);

        guest
            .memory()
            .read_exact(dirent.cast(), dents_bytes.as_mut_slice())?;

        let mut dents = unsafe { deserialize_dirents64(&dents_bytes) };
        dents.sort();
        for dent in &mut dents {
            let (d_ino, _) = determinize_inode(guest, dent.ino).await;
            dent.ino = d_ino.as_raw();
        }

        let mut dents_bytes = vec![0; dents_bytes.len()];
        let _ = unsafe { serialize_dirents64(&dents, &mut dents_bytes) };

        guest
            .memory()
            .write_exact(dirent.cast(), dents_bytes.as_slice())?;
        Ok(nb)
    }
}

#[cfg(test)]
mod procfs_wiring_guard {
    //! The procfs snapshot WIRING, guarded where the wiring lives.
    //!
    //! WHY THIS IS A SOURCE-LEVEL GUARD AND NOT A BEHAVIOURAL TEST. The thing
    //! at risk is not `ProcfsFile`'s logic -- that is exercised elsewhere. It is
    //! the CALL from each read handler into the snapshot initialiser. Those
    //! handlers are `async fn`s on the `Tool` trait taking a live `Guest`, so a
    //! unit test cannot invoke one without standing up a traced guest process;
    //! that is exactly why the only thing guarding this today is one heavyweight
    //! integration test that compiles a C probe and runs hermit.
    //!
    //! MEASURED GAP (2026-08-07, hermit 75506005d): deleting the pread64
    //! snapshot-initialisation block leaves ALL 386 detcore lib tests green.
    //! A mechanism whose only proof of life is one fixture is one deletion away
    //! from vanishing unnoticed -- the positioned-read determinism bug this
    //! wiring fixes would silently return.
    //!
    //! These assertions are deliberately narrow: they bind to the CALL, name the
    //! mechanism when they fail, and cost nothing to run. They do not claim to
    //! verify that the snapshot is correct.

    /// The production source only. `include_str!` pulls in THIS module too, so a
    /// naive scan counts the guard's own string literals and reports phantom
    /// duplicates -- it did exactly that on first run. Truncating at the guard's
    /// own header makes every assertion below immune to self-reference.
    fn production_source() -> &'static str {
        const WHOLE: &str = include_str!("files.rs");
        const GUARD: &str = "#[cfg(test)]\nmod procfs_wiring_guard {";
        match WHOLE.find(GUARD) {
            Some(cut) => &WHOLE[..cut],
            None => WHOLE,
        }
    }

    /// The body of `fn <name>` up to the next top-level `    }` at fn indent.
    fn handler_body(name: &str) -> &'static str {
        let start = production_source()
            .find(&format!("fn {}<G: Guest<Self>>", name))
            .unwrap_or_else(|| {
                panic!(
                    "procfs wiring guard: handler `{name}` not found in files.rs.\n\
                     TWO VERY DIFFERENT CAUSES, and the guard cannot tell them apart:\n\
                       (a) the handler was RENAMED or its signature changed -- the \
                     mechanism is fine, update the name in this guard; or\n\
                       (b) the handler was DELETED -- the procfs snapshot wiring is gone.\n\
                     Check which before editing. This guard binds to source text on \
                     purpose: the handlers are async `Tool` methods taking a live Guest, \
                     so nothing cheaper can observe the call. It is deliberately loud \
                     when it cannot see the code, because silently passing is the \
                     failure it exists to prevent."
                )
            });
        let rest = &production_source()[start..];
        let end = rest
            .find(
                "
    }
",
            )
            .map(|e| e + 6)
            .unwrap_or(rest.len());
        &rest[..end]
    }

    #[test]
    fn pread64_initializes_the_procfs_snapshot() {
        let body = handler_body("handle_pread64");
        assert!(
            body.contains("procfs_needs_snapshot") && body.contains("initialize_procfs_snapshot"),
            "MISSING MECHANISM: the pread64 handler no longer initialises the procfs \
             snapshot. Positioned reads will fall through to LIVE KERNEL BYTES instead of \
             the sanitized ProcfsFile snapshot, reintroducing the positioned-read \
             nondeterminism that hermit-cli/tests/procfs_positioned_determinism.rs exists \
             to catch. Restore the `procfs_needs_snapshot` -> `initialize_procfs_snapshot` \
             call in handle_pread64."
        );
    }

    #[test]
    fn read_initializes_the_procfs_snapshot() {
        let body = handler_body("handle_read");
        assert!(
            body.contains("procfs_needs_snapshot") && body.contains("initialize_procfs_snapshot"),
            "MISSING MECHANISM: the sequential read handler no longer initialises the \
             procfs snapshot. Reads of /proc will observe live kernel bytes. Restore the \
             `procfs_needs_snapshot` -> `initialize_procfs_snapshot` call in handle_read."
        );
    }

    #[test]
    fn both_read_paths_share_one_snapshot_initializer() {
        // The original defect was exactly this asymmetry: `read` consumed the
        // sanitized snapshot while `pread64` did not. Forking the logic into two
        // initialisers is how that asymmetry comes back.
        let n = production_source()
            .matches("async fn initialize_procfs_snapshot")
            .count();
        assert_eq!(
            n, 1,
            "MISSING MECHANISM: expected exactly ONE `initialize_procfs_snapshot` \
             definition so every read path shares it; found {n}. Two initialisers is how \
             read/pread64 drifted apart in the first place."
        );
        for handler in ["handle_read", "handle_pread64"] {
            assert!(
                handler_body(handler).contains("self.initialize_procfs_snapshot("),
                "MISSING MECHANISM: `{handler}` does not call the shared \
                 initialize_procfs_snapshot."
            );
        }
    }

    #[test]
    fn the_guard_can_actually_see_the_handlers() {
        // Positive control: if the extractor silently returned empty bodies the
        // three assertions above would be vacuous rather than protective.
        for handler in ["handle_read", "handle_pread64"] {
            let body = handler_body(handler);
            assert!(
                body.len() > 200 && body.contains("call.fd()"),
                "guard extractor did not find a real body for `{handler}` \
                 (len {}), so the wiring assertions would be vacuous",
                body.len()
            );
        }
    }
}

#[cfg(test)]
mod test {
    use nix::fcntl::OFlag;
    use reverie::syscalls::FromToRaw;
    use reverie::syscalls::Whence;

    use super::DETERMINISTIC_PIPE_CAPACITY_BYTES;
    use super::pipe_capacity_request_exceeds_ceiling;
    /// The ceiling is inclusive. A guest that reads the advertised
    /// `pipe-max-size` and asks for exactly that must be allowed to have it;
    /// refusing at the boundary would advertise a size that cannot be set.
    #[test]
    fn the_pipe_ceiling_admits_exactly_the_pinned_capacity() {
        assert!(!pipe_capacity_request_exceeds_ceiling(
            DETERMINISTIC_PIPE_CAPACITY_BYTES
        ));
        assert!(!pipe_capacity_request_exceeds_ceiling(
            DETERMINISTIC_PIPE_CAPACITY_BYTES - 1
        ));
        assert!(pipe_capacity_request_exceeds_ceiling(
            DETERMINISTIC_PIPE_CAPACITY_BYTES + 1
        ));
    }

    /// Shrinking stays legal. `tests/backend-parity/fixtures/pipe_capacity.c`
    /// shrinks to one page and requires the value to round-trip, so a blanket
    /// clamp to the pinned capacity would break a guest-visible contract this
    /// repository already locked.
    #[test]
    fn shrinking_is_never_refused_by_the_ceiling() {
        for requested in [1, 4096, DETERMINISTIC_PIPE_CAPACITY_BYTES / 2] {
            assert!(
                !pipe_capacity_request_exceeds_ceiling(requested),
                "shrink to {requested} must remain permitted"
            );
        }
    }

    /// The host's own ceiling is the value this change exists to stop
    /// consulting: 1048576 on a default host, 65536 on a hardened one. Both are
    /// refused now, so the guest-visible answer no longer depends on which host
    /// it is.
    #[test]
    fn host_ceilings_are_refused_identically_on_any_host() {
        for host_ceiling in [65536, 1048576] {
            assert!(
                pipe_capacity_request_exceeds_ceiling(host_ceiling),
                "{host_ceiling} must be refused regardless of the host sysctl"
            );
        }
    }

    use super::Errno;
    use super::TimerSlackBinding;
    use super::UNIX_AUTOBIND_NAME_LEN;
    use super::canonicalize_tcp_info;
    use super::classify_timer_slack_binding;
    use super::is_inherited_container_output;
    use super::parse_timer_slack_write;
    use super::pipe_capacity_failure;
    use super::random_device_lseek_result;
    use super::should_tag_sabre_internal_pipe_io;
    use super::unix_autobind_address;
    use super::unix_autobind_addrlen;
    use super::vectored_offset;
    use crate::fd::FdType;
    use crate::resources::Device;
    use crate::resources::ResourceID;

    /// This is an assumption we're making about flags.  Probably these flags can never be
    /// changed, but let's check just in case.
    #[test]
    fn linux_flags_assumptions() {
        assert_eq!(libc::SOCK_NONBLOCK, OFlag::O_NONBLOCK.bits());
        assert_eq!(libc::SOCK_CLOEXEC, OFlag::O_CLOEXEC.bits());
    }

    #[test]
    fn pipe_capacity_failure_classifies_the_errno() {
        let created = [17, 18];

        // The only success shape: Linux applied EXACTLY the capacity we asked for.
        assert_eq!(
            pipe_capacity_failure(created, Ok(i64::from(DETERMINISTIC_PIPE_CAPACITY_BYTES))),
            None
        );

        // Successful-but-wrong capacity is a pipe whose size we did not choose. That is not a
        // kernel errno, so it is reported as EIO rather than dressed up as one.
        let mismatch = pipe_capacity_failure(
            created,
            Ok(i64::from(DETERMINISTIC_PIPE_CAPACITY_BYTES) * 2),
        )
        .expect("a capacity Linux rounded away from the pin must not read as success");
        assert_eq!(mismatch.created_fds, created);
        assert_eq!(mismatch.error, Errno::EIO);

        // A real kernel errno is preserved rather than rewritten.
        let denied = pipe_capacity_failure(created, Err(Errno::EPERM))
            .expect("a kernel refusal must not read as success");
        assert_eq!(denied.created_fds, created);
        assert_eq!(denied.error, Errno::EPERM);

        // The descriptors are carried through every failure shape, because they are what the
        // caller has to close; losing them here is how they would leak.
        assert_eq!(denied.created_fds, created);
    }

    #[test]
    fn pipe_capacity_failure_closes_both_created_descriptors() {
        let mut created = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(created.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );

        let pin_result = unsafe { libc::fcntl(created[0], libc::F_SETPIPE_SZ, -1) };
        assert_eq!(pin_result, -1);
        let pin_error = Errno::last();
        // Pin the errno, not just the failure. Without this the test asserts only that
        // `fcntl` returned -1, so it would still pass if the call failed for a reason we
        // did not engineer -- an invalid `created[0]` fails EBADF, every assertion below
        // still holds, and the capacity-pin path is never exercised at all.
        //
        // EINVAL is structural here, not a property of this host. `fcntl`'s argument is an
        // `unsigned long`, so -1 arrives as ULONG_MAX; `round_pipe_size` returns 0 for any
        // size above 2^31, and `pipe_set_size` maps that 0 to -EINVAL BEFORE it consults
        // `CAP_SYS_RESOURCE`. So the result does not depend on privilege or on
        // `/proc/sys/fs/pipe-max-size`.
        assert_eq!(pin_error, Errno::EINVAL);

        let failure = pipe_capacity_failure(created, Err(pin_error))
            .expect("the forced capacity-pin failure must enter the cleanup path");
        for close in failure.close_syscalls() {
            assert_eq!(unsafe { libc::close(close.fd()) }, 0);
        }

        for fd in created {
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
            assert_eq!(Errno::last(), Errno::EBADF);
        }
    }

    #[test]
    fn sabre_pipe_marker_requires_nonblockize_retry_semantics() {
        assert!(should_tag_sabre_internal_pipe_io(
            true,
            FdType::Pipe,
            true,
            false
        ));
        assert!(!should_tag_sabre_internal_pipe_io(
            true,
            FdType::Pipe,
            true,
            true
        ));
        assert!(!should_tag_sabre_internal_pipe_io(
            true,
            FdType::Pipe,
            false,
            false
        ));
        assert!(!should_tag_sabre_internal_pipe_io(
            false,
            FdType::Pipe,
            true,
            false
        ));
        assert!(!should_tag_sabre_internal_pipe_io(
            true,
            FdType::Regular,
            true,
            false
        ));
    }

    #[test]
    fn random_device_lseek_matches_linux_noop_llseek() {
        for whence in [
            Whence::SEEK_SET,
            Whence::SEEK_CUR,
            Whence::SEEK_END,
            Whence::SEEK_DATA,
            Whence::SEEK_HOLE,
        ] {
            for status_flags in [
                OFlag::empty().bits(),
                OFlag::O_WRONLY.bits(),
                OFlag::O_RDWR.bits(),
            ] {
                assert_eq!(random_device_lseek_result(status_flags, whence), Ok(0));
            }
            assert_eq!(
                random_device_lseek_result(OFlag::O_PATH.bits(), whence),
                Err(Errno::EBADF)
            );
        }
        assert_eq!(
            random_device_lseek_result(OFlag::empty().bits(), Whence::from_raw(99)),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            random_device_lseek_result(OFlag::O_PATH.bits(), Whence::from_raw(99)),
            Err(Errno::EBADF)
        );
    }

    #[test]
    fn timer_slack_write_parser_matches_decimal_procfs_contract() {
        assert_eq!(parse_timer_slack_write(b"0"), Ok(0));
        assert_eq!(parse_timer_slack_write(b"+123\n"), Ok(123));
        assert_eq!(parse_timer_slack_write(b"456\0ignored"), Ok(456));
        assert_eq!(
            parse_timer_slack_write(u64::MAX.to_string().as_bytes()),
            Ok(u64::MAX)
        );

        for invalid in [b"".as_slice(), b"+", b"-1", b" 1", b"1 ", b"1\n2", b"0x10"] {
            assert_eq!(parse_timer_slack_write(invalid), Err(Errno::EINVAL));
        }
        assert_eq!(
            parse_timer_slack_write(b"18446744073709551616"),
            Err(Errno::ERANGE)
        );
    }

    #[test]
    fn timer_slack_vectored_offset_preserves_minus_one_sentinel() {
        assert_eq!(vectored_offset(u64::MAX, u64::MAX), -1);
        assert_eq!(vectored_offset(0, 0), 0);
        assert_eq!(vectored_offset(7, 0), 7);
    }

    #[test]
    fn timer_slack_binding_rejects_exit_reuse_and_other_tasks() {
        let binding = TimerSlackBinding {
            target: 202,
            device: 11,
            inode: 22,
        };
        assert_eq!(
            classify_timer_slack_binding(binding, Some((11, 22)), 202),
            Ok(())
        );
        assert_eq!(
            classify_timer_slack_binding(binding, Some((11, 22)), 303),
            Err(Errno::EPERM)
        );
        assert_eq!(
            classify_timer_slack_binding(binding, None, 202),
            Err(Errno::ESRCH)
        );
        assert_eq!(
            classify_timer_slack_binding(binding, Some((11, 23)), 202),
            Err(Errno::ESRCH),
            "a recycled numeric TID must not revive an old proc inode"
        );
    }

    #[test]
    fn only_inherited_container_output_is_nonseekable() {
        assert!(is_inherited_container_output(Some(ResourceID::Device(
            Device::ContainerStdout
        ))));
        assert!(is_inherited_container_output(Some(ResourceID::Device(
            Device::ContainerStderr
        ))));
        assert!(!is_inherited_container_output(Some(ResourceID::Device(
            Device::ContainerStdin
        ))));
        assert!(!is_inherited_container_output(None));
    }

    #[test]
    fn unix_autobind_address_matches_linux_shape() {
        let address = unix_autobind_address(0x2af);
        assert_eq!(address.sun_family, libc::AF_UNIX as libc::sa_family_t);
        assert_eq!(address.sun_path[0], 0);
        let name = address.sun_path[1..UNIX_AUTOBIND_NAME_LEN]
            .iter()
            .map(|byte| *byte as u8)
            .collect::<Vec<_>>();
        assert_eq!(name, b"002af");
        assert_eq!(
            unix_autobind_addrlen() as usize,
            std::mem::offset_of!(libc::sockaddr_un, sun_path) + UNIX_AUTOBIND_NAME_LEN
        );
    }

    #[test]
    fn tcp_info_retains_only_logical_connection_header() {
        let mut info = [0xff; 16];
        canonicalize_tcp_info(&mut info);

        for (offset, byte) in info.into_iter().enumerate() {
            let expected = if matches!(offset, 0 | 1 | 5 | 6) {
                0xff
            } else {
                0
            };
            assert_eq!(byte, expected, "unexpected byte at offset {offset}");
        }

        for len in 0..8 {
            canonicalize_tcp_info(&mut [0xff; 8][..len]);
        }
    }
}
