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
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use nix::fcntl::AtFlags;
use nix::fcntl::OFlag;
use rand::RngExt as _;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::AddrSlice;
use reverie::syscalls::AddrSliceMut;
use reverie::syscalls::Errno;
use reverie::syscalls::FcntlCmd::*;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ProtFlags;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::SockFlag;
use reverie::syscalls::StatPtr;
use reverie::syscalls::StatxMask;
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
use crate::fifo_peer::FifoPeer;
use crate::procfs::MountInfoSnapshot;
use crate::procfs::ProcfsFile;
use crate::procfs::ProcfsSnapshotContext;
use crate::procfs::is_absent_cpu_entry;
use crate::procfs::levels;
use crate::procfs_inode;
use crate::procfs_inode::DirectoryInodes;
use crate::procfs_inode::ProcfsDirectory;
use crate::procfs_inode::ProcfsName;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Device;
use crate::resources::HOST_TIMED_INTERNAL_PIPE_IO_FYI;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::HostTimedSignalScope;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::scheduler::terminal_signals;
use crate::stat::*;
use crate::syscalls::guest_siginfo::GuestSiginfo;
use crate::syscalls::threads::kernel_sigset_bit;
use crate::tool_global::*;
use crate::tool_local::CapturedDetFdInstallError;
use crate::tool_local::Detcore;
use crate::tool_local::finish_partial_record_or_replay_write;
use crate::types::*;

pub(crate) mod procfs_levels;
pub(crate) mod procfs_numbers;
pub(crate) mod rendered_procfs;

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

/// RNG vectors use either the shared stream or an independent explicit offset.
struct RandomVectoredRead {
    address: usize,
    count: usize,
    offset: Option<u64>,
    flags: i32,
}

/// Apply Linux's checks after complete iovec import, before touching output.
fn validate_random_vector_read(offset: Option<u64>, total: usize, flags: i32) -> Result<(), Errno> {
    if total == 0 {
        return Ok(());
    }
    if let Some(offset) = offset
        && offset
            .checked_add(total as u64)
            .is_none_or(|end| end > i64::MAX as u64)
    {
        return Err(Errno::EINVAL);
    }
    // Linux 7.1's RWF_NOSIGNAL is newer than the pinned libc crate. Native
    // random-device controls accept it, including with RWF_NOWAIT.
    const RWF_NOSIGNAL: i32 = 0x100;
    const KNOWN: i32 = libc::RWF_HIPRI
        | libc::RWF_DSYNC
        | libc::RWF_SYNC
        | libc::RWF_NOWAIT
        | libc::RWF_APPEND
        | libc::RWF_NOAPPEND
        | libc::RWF_ATOMIC
        | libc::RWF_DONTCACHE
        | RWF_NOSIGNAL;
    // Unknown flags win over conflicting recognized flags, while recognized
    // but unsupported ATOMIC/DONTCACHE are rejected after APPEND/NOAPPEND.
    if flags & !KNOWN != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    if flags & libc::RWF_APPEND != 0 && flags & libc::RWF_NOAPPEND != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & (libc::RWF_ATOMIC | libc::RWF_DONTCACHE) != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    Ok(())
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

/// Copy sanitized procfs bytes to the caller's buffer as the kernel's
/// `copy_to_user` does: through the guest's page protections, stopping at the
/// first byte that cannot be written. Returns the bytes copied, or `EFAULT`
/// when none could be. A debugger write is not equivalent: the ptrace
/// backend writes exactly eight bytes with `PTRACE_POKEDATA`, which ignores
/// `PROT_READ` and `PROT_NONE`.
fn copy_procfs_output<M: MemoryAccess>(
    memory: &mut M,
    destination: Option<AddrMut<'_, u8>>,
    bytes: &[u8],
) -> Result<usize, Error> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let destination = destination.ok_or(Errno::EFAULT)?;
    let mut copied = 0;
    while copied < bytes.len() {
        let Some(address) = destination
            .as_raw()
            .checked_add(copied)
            .and_then(AddrMut::<u8>::from_raw)
        else {
            break;
        };
        match memory.write_with_user_access(address, &bytes[copied..]) {
            Ok(0) | Err(Errno::EFAULT) => break,
            Ok(n) => copied += n,
            // Any other failure is the backend's, not a guest fault: report it
            // as a tool error and commit nothing.
            Err(errno) => {
                return Err(Error::Tool(anyhow::anyhow!(
                    "procfs read publication failed after {copied} bytes: {errno}"
                )));
            }
        }
    }
    if copied == 0 {
        Err(Errno::EFAULT.into())
    } else {
        Ok(copied)
    }
}

/// Whether a descriptor with status flags `status` can read: not `O_PATH`,
/// and opened for reading (`O_RDONLY` or `O_RDWR`; not `O_WRONLY`, nor access
/// mode 3, which grants neither).
fn can_read(status: i32) -> bool {
    status & libc::O_PATH == 0 && matches!(status & libc::O_ACCMODE, libc::O_RDONLY | libc::O_RDWR)
}

/// Copy the bytes of a read of a procfs file as its read handler does: a
/// `seq_file` copies up to the first byte it cannot write, and a sysctl
/// (`faults_on_a_short_copy`) copies all of them or fails with `EFAULT`, so
/// that the caller commits nothing.
fn copy_procfs_read<M: MemoryAccess>(
    memory: &mut M,
    destination: Option<AddrMut<'_, u8>>,
    bytes: &[u8],
    faults_on_a_short_copy: bool,
) -> Result<usize, Error> {
    let copied = copy_procfs_output(memory, destination, bytes)?;
    if faults_on_a_short_copy && copied < bytes.len() {
        return Err(Errno::EFAULT.into());
    }
    Ok(copied)
}

/// The `RWF_*` flags of a vectored read of a procfs file Detcore renders, as
/// `kiocb_set_rw_flags` checks them for a file with `read_iter` and without
/// `FMODE_NOWAIT` or `FOP_DONTCACHE`: an unknown flag, then `RWF_APPEND` with
/// `RWF_NOAPPEND` (`EINVAL`), then `RWF_NOWAIT`, `RWF_ATOMIC` (a read) and
/// `RWF_DONTCACHE` (`EOPNOTSUPP`). Measured on Linux 7.1 for `/proc/version`
/// and the sysctls, which also accept `RWF_NOSIGNAL`. A file without
/// `read_iter` (`/proc/modules`, `/proc/swaps`) is read by
/// `do_loop_readv_writev`, which refuses every flag but `RWF_HIPRI`.
fn validate_procfs_vector_flags(flags: i32, read_iter: bool) -> Result<(), Errno> {
    if !read_iter {
        return if flags & !libc::RWF_HIPRI != 0 {
            Err(Errno::EOPNOTSUPP)
        } else {
            Ok(())
        };
    }
    // Linux 7.1's RWF_NOSIGNAL is newer than the pinned libc crate.
    const RWF_NOSIGNAL: i32 = 0x100;
    const KNOWN: i32 = libc::RWF_HIPRI
        | libc::RWF_DSYNC
        | libc::RWF_SYNC
        | libc::RWF_NOWAIT
        | libc::RWF_APPEND
        | libc::RWF_NOAPPEND
        | libc::RWF_ATOMIC
        | libc::RWF_DONTCACHE
        | RWF_NOSIGNAL;
    if flags & !KNOWN != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    if flags & libc::RWF_APPEND != 0 && flags & libc::RWF_NOAPPEND != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & (libc::RWF_NOWAIT | libc::RWF_ATOMIC | libc::RWF_DONTCACHE) != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    Ok(())
}

/// Check the caller's whole requested range as `vfs_read`'s `access_ok` does
/// before the file is read, so an address range beyond the user address limit
/// is `EFAULT` even at EOF, when nothing would be copied. A procfs snapshot read
/// never hands that range to the kernel.
///
/// `limit` is asked for the guest's user address limit only when the answer
/// can matter, so a count that no limit admits is `EFAULT` before any query
/// can fail.
fn validate_procfs_destination(
    limit: impl FnOnce() -> Result<crate::iovecs::UserAddressLimit, Error>,
    destination: Option<AddrMut<'_, u8>>,
    len: usize,
) -> Result<(), Error> {
    // No count past isize::MAX fits below a user address limit, so `read`'s
    // `access_ok` refuses it, whatever the limit.
    if isize::try_from(len).is_err() {
        return Err(Errno::EFAULT.into());
    }
    // Two descriptors keep the range uncapped, as `access_ok` sees it; a
    // single one may be capped at MAX_RW_COUNT first (`import_ubuf`).
    limit()?.validate(&[
        crate::iovecs::ImportedIovec {
            base: destination.map_or(0, |address| address.as_raw()),
            len,
        },
        crate::iovecs::ImportedIovec { base: 0, len: 0 },
    ])
}

/// Save the bytes at `scratch` into `original` if the guest's own protections
/// let the capture read land there: the range is readable, and writing the
/// same bytes back through the guest's protections copies all of them.
fn save_procfs_scratch<M: MemoryAccess>(
    memory: &mut M,
    scratch: AddrMut<'_, u8>,
    original: &mut [u8],
) -> bool {
    read_guest_exact(memory, scratch, original).is_ok()
        && memory
            .write_with_user_access(scratch, original)
            .is_ok_and(|written| written == original.len())
}

/// Read exactly `buf.len()` bytes at `addr`, in one `read_vectored` of that
/// length, or fail `EFAULT` if not all of them can be read. `read` and
/// `read_exact` may widen a short read (ptrace turns up to eight bytes into one
/// eight-byte `PTRACE_PEEKDATA`), which fails when a valid extent ends less
/// than eight bytes before an unmapped page.
pub(crate) fn read_guest_exact<M: MemoryAccess>(
    memory: &M,
    addr: AddrMut<'_, u8>,
    buf: &mut [u8],
) -> Result<(), Errno> {
    let len = buf.len();
    let remote = unsafe { AddrSlice::from_raw_parts(addr.into(), len) };
    let remote = [unsafe { remote.as_ioslice() }];
    let mut local = [std::io::IoSliceMut::new(buf)];
    match memory.read_vectored(&remote, &mut local) {
        Ok(copied) if copied == len => Ok(()),
        Ok(_) => Err(Errno::EFAULT),
        Err(errno) => Err(errno),
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

/// The capacity a guest's `F_GETPIPE_SZ` reports for a pipe or FIFO whose
/// host capacity is `host`: never more than the deterministic ceiling.
///
/// A pipe Hermit did not create (an inherited stdout, a FIFO whose other end
/// is on the host) keeps the capacity the host gave it: 64 KiB normally, but
/// two pages (8192) once the user crosses `pipe-user-pages-soft`, which host
/// load decides. Passed through, that made the same guest read 65536 in one
/// run of `--verify` and 8192 in the other (`/bin/cat` on DBT, the validate of
/// hermit 2cd56771). A pipe can never hold more than the ceiling a guest can
/// raise it to, so above it the ceiling is reported, as `pipe-max-size` is. A
/// capacity at or below it is the one a pipe Hermit created was pinned to, or
/// one the guest set itself, and is reported as it is. Host load cannot make
/// a pipe smaller than the ceiling on Linux 5.14 and later: the quota fallback
/// is exactly two pages, and past the hard limit Linux refuses the pipe
/// instead. So an inherited pipe below it is one its creator sized on purpose,
/// the same in every run. Before 5.14 the fallback was one page; the creation
/// pin already needs 5.14 there, since under the same quota its growth of a
/// one-page pipe to the ceiling is refused and the run stops
/// (`pipe_capacity_failure`).
///
/// Only the reported value is made deterministic. A write to a pipe drained on
/// the host still completes as far as its real capacity and that reader allow.
pub(crate) fn reported_pipe_capacity(host: i64) -> i64 {
    host.min(i64::from(DETERMINISTIC_PIPE_CAPACITY_BYTES))
}

/// SIGIO and SIGURG, the signals Linux sends to a descriptor's owner when
/// data, out-of-band data, a lease break, or a directory change arrives.
fn async_io_signals() -> u64 {
    kernel_sigset_bit(libc::SIGIO) | kernel_sigset_bit(libc::SIGURG)
}

/// The signals an `fcntl` command lets Linux send at a moment set by host
/// timing, as a kernel sigset; 0 for every other command. `F_SETOWN` and
/// `F_SETOWN_EX` name the owner, `F_SETLEASE` and `F_NOTIFY` make the caller
/// the owner, `O_ASYNC` enables SIGIO, and `F_SETSIG` replaces SIGIO with
/// another signal.
fn fcntl_host_timed_signals(cmd: syscalls::FcntlCmd<'_>) -> u64 {
    match cmd {
        F_SETFL(flags) if flags & libc::O_ASYNC != 0 => async_io_signals(),
        F_SETOWN | F_SETOWN_EX(_) | F_SETLEASE(_) | F_NOTIFY(_) => async_io_signals(),
        F_SETSIG(signal) => async_io_signals() | kernel_sigset_bit(signal),
        _ => 0,
    }
}

/// The signals an `ioctl` request lets Linux send at a moment set by host
/// timing, as a kernel sigset; 0 for every other request. `FIOASYNC` enables
/// SIGIO, and `FIOSETOWN` and `SIOCSPGRP` name the owner. `TIOCSCTTY` makes
/// the terminal the caller's controlling terminal, `TIOCGPTPEER` without
/// `O_NOCTTY` opens a pseudoterminal's other end as `open` does and so can
/// make it one (`open_can_acquire_controlling_terminal`), and `TIOCSPGRP`
/// names the terminal's foreground process group, so the terminal can then
/// signal the session and that group (`terminal_signals`). `TIOCNOTTY` and
/// `TIOCSWINSZ` are not among them: the signals they send, they send inside
/// the caller's call, in the caller's turn.
fn ioctl_host_timed_signals(request: syscalls::ioctl::Request<'_>) -> u64 {
    match request {
        syscalls::ioctl::Request::FIOASYNC(_)
        | syscalls::ioctl::Request::FIOSETOWN(_)
        | syscalls::ioctl::Request::SIOCSPGRP(_) => async_io_signals(),
        syscalls::ioctl::Request::TIOCSCTTY(_) | syscalls::ioctl::Request::TIOCSPGRP(_) => {
            terminal_signals()
        }
        syscalls::ioctl::Request::TIOCGPTPEER(flags)
            if open_can_acquire_controlling_terminal(OFlag::from_bits_truncate(flags)) =>
        {
            terminal_signals()
        }
        _ => 0,
    }
}

/// Whether an `open` with these flags can make the terminal it opens the
/// caller's controlling terminal. Linux's `tty_open` does that for a session
/// leader that has none, unless `O_NOCTTY` is given; an `O_PATH` open never
/// reaches `tty_open`.
fn open_can_acquire_controlling_terminal(flags: OFlag) -> bool {
    !flags.intersects(OFlag::O_NOCTTY | OFlag::O_PATH)
}

/// Whether the character device `rdev` is a terminal that an `open` can make
/// its caller's controlling terminal. `tty_drivers` is the text of
/// `/proc/tty/drivers`, one line per range of device numbers a terminal
/// driver serves, ending in the major number, the minor number or range, and
/// the driver's type. A device no line names is not a terminal. Linux's
/// `tty_open` never makes `/dev/console` (5:1), `/dev/vc/0` (4:0) or a
/// pseudoterminal master controlling, and `/dev/ptmx` (5:2) opens a master
/// without reaching `tty_open`. `/dev/tty` (5:0) stays in: it reopens the
/// caller's controlling terminal, and if a hangup clears that terminal during
/// the `open`, the `open` can make it controlling again. A table that could
/// not be read (`None`), or a line that does not parse when no other line
/// names the device, answers true.
fn terminal_can_become_controlling(rdev: libc::dev_t, tty_drivers: Option<&str>) -> bool {
    let (major, minor) = (libc::major(rdev), libc::minor(rdev));
    if matches!((major, minor), (5, 1) | (5, 2) | (4, 0)) {
        return false;
    }
    let Some(table) = tty_drivers else {
        return true;
    };
    let mut unparsed = false;
    for line in table.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let range = match fields[..] {
            [.., line_major, minors, kind] => {
                line_major.parse::<u32>().ok().and_then(|line_major| {
                    let (first, last) = minors.split_once('-').unwrap_or((minors, minors));
                    Some((
                        line_major,
                        first.parse::<u32>().ok()?,
                        last.parse::<u32>().ok()?,
                        kind,
                    ))
                })
            }
            _ => None,
        };
        match range {
            Some((line_major, first, last, kind))
                if line_major == major && (first..=last).contains(&minor) =>
            {
                return kind != "pty:master";
            }
            Some(_) => {}
            None => unparsed = true,
        }
    }
    unparsed
}

/// Whether an `open` in process `pid`, whose `/proc/<pid>/stat` lists
/// `session`, of the character device `rdev` may have made that device
/// `pid`'s controlling terminal: `pid` leads its session and the device is a
/// terminal that can become controlling (`terminal_can_become_controlling`).
/// Neither fact is the controlling terminal procfs lists, which a hangup of
/// the terminal clears at any moment, so the answer does not depend on
/// whether a hangup came before or after the read. A session leader that
/// reopens the terminal it already controls, or opens a terminal while it
/// controls another, also answers true.
fn may_have_acquired_controlling_terminal(
    pid: i32,
    session: i32,
    rdev: libc::dev_t,
    tty_drivers: Option<&str>,
) -> bool {
    session == pid && terminal_can_become_controlling(rdev, tty_drivers)
}

/// Whether the `open` in process `pid` that returned `fd` may have made the
/// opened terminal `pid`'s controlling terminal
/// (`may_have_acquired_controlling_terminal`). `host_stat` is the
/// descriptor's stat when the caller already read it. Every host read that
/// fails answers true: a needless record only holds the terminal's signals
/// (`terminal_signals`) to the end of the gated waits that follow, while a
/// missed one lets the terminal interrupt one at a turn the host chose.
fn opened_controlling_terminal(pid: i32, fd: RawFd, host_stat: Option<&libc::stat>) -> bool {
    let rdev = match host_stat {
        Some(stat) if stat.st_mode & libc::S_IFMT == libc::S_IFCHR => stat.st_rdev,
        Some(_) => return false,
        None => match std::fs::metadata(format!("/proc/{pid}/fd/{fd}")) {
            Ok(metadata) if metadata.file_type().is_char_device() => metadata.rdev(),
            Ok(_) => return false,
            Err(_) => return true,
        },
    };
    let session = match procfs::process::Process::new(pid).and_then(|process| process.stat()) {
        Ok(stat) => stat.session,
        Err(_) => return true,
    };
    let tty_drivers = std::fs::read_to_string("/proc/tty/drivers").ok();
    may_have_acquired_controlling_terminal(pid, session, rdev, tty_drivers.as_deref())
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

fn should_tag_host_timed_internal_pipe_io(
    internal_pipe_turns_are_host_timed: bool,
    fd_type: FdType,
    physically_nonblocking: bool,
    logically_nonblocking: bool,
) -> bool {
    internal_pipe_turns_are_host_timed
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

fn require_random_device_read_access(status_flags: i32) -> Result<(), Errno> {
    if status_flags & libc::O_PATH != 0
        || !matches!(
            status_flags & libc::O_ACCMODE,
            libc::O_RDONLY | libc::O_RDWR
        )
    {
        Err(Errno::EBADF)
    } else {
        Ok(())
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

/// Does the new guest descriptor `fd` reopen an anonymous pipe that the same
/// process already holds as a scheduler-managed pipe (`managed_pipe_fds`)?
///
/// Both `/proc/<pid>/fd/<fd>` links read `pipe:[<inode>]` for the same pipe.
/// A named FIFO links to its path, and on ptrace a host pipe is never in
/// `managed_pipe_fds` (see `scheduler_managed_pipe_fds` for SaBRe), so neither
/// matches: a host writer is outside the scheduler and must not be polled as if
/// it were a guest
/// (<https://github.com/rrnewton/hermit/pull/3534#issuecomment-5962369261>).
fn reopens_scheduler_managed_pipe(
    backend: &reverie::BackendCapabilities,
    pid: i32,
    fd: RawFd,
    managed_pipe_fds: &[RawFd],
) -> bool {
    // Under KVM `/proc/<pid>` is an unrelated host task, if any, and its
    // links say nothing about this guest's pipes.
    if !crate::procfs_inode::tool_can_name_guest_descriptors(backend) {
        return false;
    }
    let link = |fd: RawFd| std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok();
    let Some(opened) = link(fd) else {
        return false;
    };
    if !opened
        .to_str()
        .is_some_and(|name| name.starts_with("pipe:["))
    {
        return false;
    }
    managed_pipe_fds
        .iter()
        .any(|&held| held != fd && link(held).as_ref() == Some(&opened))
}

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
/// unavailable), in which case the lexical result stands unchanged. Also
/// `None` under KVM, where `/proc/<pid>` does not name the guest (see
/// [`crate::procfs_inode::tool_can_name_guest_descriptors`]).
fn resolved_open_path(
    backend: &reverie::BackendCapabilities,
    pid: i32,
    fd: RawFd,
) -> Option<PathBuf> {
    if !crate::procfs_inode::tool_can_name_guest_descriptors(backend) {
        return None;
    }
    let link = std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()?;
    // A deleted or anonymous target is not a stable object name.
    link.is_absolute().then_some(link)
}

/// The scope `pidfd_send_signal`'s flags select (Linux's `pidfd_send_signal`
/// since 6.9; <https://github.com/rrnewton/hermit/issues/4108>).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PidfdSignalScope {
    /// No flag, or `PIDFD_SIGNAL_THREAD_GROUP`: the process (Detcore's
    /// `pidfd_open` makes no thread pidfds).
    Process,
    /// `PIDFD_SIGNAL_THREAD`: the thread whose id is the pid.
    Thread,
    /// `PIDFD_SIGNAL_PROCESS_GROUP`: the process's process group.
    ProcessGroup,
}

const PIDFD_SIGNAL_THREAD: u32 = 1 << 0;
const PIDFD_SIGNAL_THREAD_GROUP: u32 = 1 << 1;
const PIDFD_SIGNAL_PROCESS_GROUP: u32 = 1 << 2;

/// `flags`' scope, or EINVAL as Linux answers: for a bit outside the three
/// scope flags, or for more than one of them.
pub(crate) fn pidfd_signal_scope(flags: u32) -> Result<PidfdSignalScope, Errno> {
    match flags {
        0 | PIDFD_SIGNAL_THREAD_GROUP => Ok(PidfdSignalScope::Process),
        PIDFD_SIGNAL_THREAD => Ok(PidfdSignalScope::Thread),
        PIDFD_SIGNAL_PROCESS_GROUP => Ok(PidfdSignalScope::ProcessGroup),
        _ => Err(Errno::EINVAL),
    }
}

/// `pidfd_send_signal`'s sentinel for the calling thread (Linux 6.15 and later).
const PIDFD_SELF_THREAD: RawFd = -10000;
/// `pidfd_send_signal`'s sentinel for the calling thread's thread group.
const PIDFD_SELF_THREAD_GROUP: RawFd = -10001;

/// The task a `PIDFD_SELF_*` sentinel names, and the scope Linux gives it when
/// no flag is set: the calling thread (thread-directed) or its thread group
/// (process-directed); `None` for any other descriptor
/// (https://github.com/rrnewton/hermit/issues/4147).
fn pidfd_self_target<T, G: Guest<Detcore<T>>>(
    guest: &mut G,
    pidfd: RawFd,
) -> Option<(DetPid, PidfdSignalScope)>
where
    T: RecordOrReplay,
{
    let state = guest.thread_state();
    match pidfd {
        PIDFD_SELF_THREAD => Some((
            DetPid::from_raw(state.dettid.as_raw()),
            PidfdSignalScope::Thread,
        )),
        PIDFD_SELF_THREAD_GROUP => Some((state.detpid?, PidfdSignalScope::Process)),
        _ => None,
    }
}

/// What a descriptor that is not a modeled pidfd names for `pidfd_send_signal`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProcessDirectoryDescriptor {
    /// A `/proc/<pid>` directory of the guest's procfs: Linux takes it as the
    /// process `<pid>`.
    Process(DetPid),
    /// Any other file, which Linux answers EBADF.
    NotAProcess,
    /// Detcore cannot tell; the reason names why.
    Unknown(&'static str),
}

/// Facts the tracer reads about a guest descriptor (`process_directory_descriptor`).
#[derive(Clone, Debug, Default)]
pub(crate) struct DescriptorFacts {
    /// The descriptor's `/proc/<tid>/fd/<fd>` link, as the kernel spells it.
    pub(crate) link: PathBuf,
    /// Whether the file is on procfs.
    pub(crate) on_procfs: bool,
    /// Whether the file is a directory.
    pub(crate) is_dir: bool,
    /// The file's device, and its parent directory's device and inode.
    pub(crate) dev: u64,
    pub(crate) parent: Option<(u64, u64)>,
    /// The device of the procfs mounted at the guest's `/proc`.
    pub(crate) guest_proc_dev: Option<u64>,
}

/// The inode of a procfs root directory (Linux's `PROC_ROOT_INO`).
const PROC_ROOT_INO: u64 = 1;

/// Classify a descriptor from `facts` (`None`: unreadable). Linux's
/// `tgid_pidfd_to_pid` accepts exactly a procfs process directory, whose name
/// is its pid in the procfs instance's pid namespace and whose parent is the
/// procfs root. That instance must be the guest's own, at its `/proc`, so the
/// pid is a guest pid. A pidfd Detcore did not open cannot be classified.
pub(crate) fn classify_process_directory(
    facts: Option<&DescriptorFacts>,
) -> ProcessDirectoryDescriptor {
    use ProcessDirectoryDescriptor as D;
    let Some(facts) = facts else {
        return D::Unknown("Hermit cannot read what the descriptor names");
    };
    let link = facts.link.to_string_lossy();
    if link == "anon_inode:[pidfd]" || link.starts_with("pidfd:") {
        return D::Unknown("it is a pidfd that Detcore did not open with pidfd_open");
    }
    if !facts.on_procfs {
        return D::NotAProcess;
    }
    if !facts.is_dir {
        return D::NotAProcess;
    }
    let name = facts
        .link
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A reaped task's directory is spelled with " (deleted)", and its ".."
    // cannot be read (ESRCH); Linux still takes it as the task, and answers
    // ESRCH for it. The spelling is then the only evidence, so it must be a
    // top-level entry of the guest's /proc.
    let name = match name.strip_suffix(" (deleted)") {
        Some(name) if facts.parent.is_none() => {
            if facts.link.parent() != Some(Path::new("/proc")) {
                return if facts.link.starts_with("/proc/") {
                    D::NotAProcess
                } else {
                    D::Unknown("it names a reaped task outside the guest's /proc")
                };
            }
            name.to_owned()
        }
        _ if facts.parent != Some((facts.dev, PROC_ROOT_INO)) => return D::NotAProcess,
        _ => name,
    };
    let Ok(pid) = name.parse::<i32>() else {
        return D::NotAProcess;
    };
    if pid <= 0 || name.starts_with(['+', '0']) {
        return D::NotAProcess;
    }
    if facts.guest_proc_dev != Some(facts.dev) {
        return D::Unknown("it is a procfs that is not the guest's /proc");
    }
    D::Process(DetPid::from_raw(pid))
}

/// Read the facts for the guest descriptor `fd` of thread `tid` through
/// `/proc/<tid>/fd/<fd>` and classify it (`classify_process_directory`).
fn process_directory_descriptor(tid: i32, fd: RawFd) -> ProcessDirectoryDescriptor {
    let facts = (|| {
        let (link, on_procfs) = descriptor_identity(tid, fd)?;
        let target = std::fs::metadata(format!("/proc/{tid}/fd/{fd}")).ok()?;
        let parent = std::fs::metadata(format!("/proc/{tid}/fd/{fd}/.."))
            .ok()
            .map(|parent| (parent.dev(), parent.ino()));
        let guest_proc_dev = std::fs::metadata(format!("/proc/{tid}/root/proc"))
            .ok()
            .filter(|proc| proc.ino() == PROC_ROOT_INO)
            .map(|proc| proc.dev());
        Some(DescriptorFacts {
            link,
            on_procfs,
            is_dir: target.is_dir(),
            dev: target.dev(),
            parent: if target.is_dir() { parent } else { None },
            guest_proc_dev,
        })
    })();
    classify_process_directory(facts.as_ref())
}

/// The host's default huge page size (`Hugepagesize` in /proc/meminfo), the
/// size of a MAP_HUGETLB mapping that names none, or 2 MiB, the x86_64
/// default, when it cannot be read (see [`may_change_untraced_code`]).
fn default_huge_page_size() -> u64 {
    static SIZE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|meminfo| {
                let line = meminfo
                    .lines()
                    .find(|line| line.starts_with("Hugepagesize:"))?;
                let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
                kib.checked_mul(1024)
            })
            .unwrap_or(2 << 20)
    })
}

/// What a guest descriptor is, read through the opening thread's
/// `/proc/<tid>/fd/<fd>`: its link as the kernel spells it, and whether the
/// file is on procfs (see [`may_write_process_memory`]). Read through the
/// thread because its descriptor table may not be its leader's (a thread
/// cloned without CLONE_FILES). `None` when either cannot be read.
fn descriptor_identity(tid: i32, fd: RawFd) -> Option<(PathBuf, bool)> {
    let path = format!("/proc/{tid}/fd/{fd}");
    let link = std::fs::read_link(&path).ok()?;
    let path = std::ffi::CString::new(path).ok()?;
    let mut buf = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `buf` is large enough for statfs.
    if unsafe { libc::statfs(path.as_ptr(), buf.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statfs succeeded, so it filled `buf`.
    let on_procfs = unsafe { buf.assume_init() }.f_type as u64
        == reverie::syscalls::linux_abi::PROC_SUPER_MAGIC as u64;
    Some((link, on_procfs))
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

/// Whether removing a name of the file `stat` describes leaves it with no
/// name: a directory has only one, and a non-directory with one link loses
/// its last. A host may then give its inode to another file
/// (<https://github.com/rrnewton/hermit/issues/3840>).
fn retires_on_removal(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFDIR || stat.st_nlink == 1
}

/// The path to report for one operand of a successful namespace change (see
/// `Detcore::record_host_namespace_change`): the operand made absolute, or
/// `/` when it could not be read or made absolute. A tracer that may not read
/// a non-dumpable guest's memory, for one, cannot read it while the change
/// itself succeeds. `/` is an ancestor of every path, so no host input change
/// is named in that run, rather than one the guest may have made.
fn rebound_path(observed: Option<PathBuf>) -> String {
    match observed {
        Some(path) if path.is_absolute() => path.to_string_lossy().into_owned(),
        _ => String::from("/"),
    }
}

/// Whether the raw call `number(args)` may change the guest memory in
/// `range`, `[start, end)` (see `Config::untraced_code_range`):
/// - a fixed mapping over any part of it: `mmap` with MAP_FIXED, `mremap`
///   with MREMAP_FIXED, `shmat` with SHM_REMAP;
/// - a protection, advice or unmapping over any part of it: `mprotect`,
///   `pkey_mprotect`, `madvise`, `munmap`, the old range of `mremap`.
///
/// Linux takes page-aligned addresses for these calls and rounds a length up
/// to whole pages, and `range` is whole pages, so the unrounded interval
/// overlaps it exactly when the rounded one does. A MAP_HUGETLB `mmap` rounds
/// to its own page size on both ends, so it is widened to the size its
/// MAP_HUGE_* bits name, or else to `default_huge_page` (the host's
/// `Hugepagesize`). A zero length counts as its first byte, and a `shmat` as
/// reaching the end of the address space, since its size is the segment's.
/// A mapping without a fixed address never lands on an existing one, and the
/// guest's `process_vm_writev`, `ptrace` and `remap_file_pages` are refused.
/// Scalar arguments are never read as addresses, so an ordinary call is
/// never counted.
pub(crate) fn may_change_untraced_code(
    number: Sysno,
    args: &syscalls::SyscallArgs,
    (start, end): (u64, u64),
    default_huge_page: u64,
) -> bool {
    let [arg0, arg1, arg2, arg3, arg4] =
        [args.arg0, args.arg1, args.arg2, args.arg3, args.arg4].map(|arg| arg as u64);
    let overlaps = |addr: u64, len: u64| addr < end && addr.saturating_add(len.max(1)) > start;
    let overlaps_huge = |addr: u64, len: u64, page: u64| {
        let page = page.max(1);
        let first = addr - addr % page;
        let last = addr
            .saturating_add(len.max(1))
            .div_ceil(page)
            .saturating_mul(page);
        overlaps(first, last.saturating_sub(first).max(1))
    };
    let huge_page = |flags: u64| match (flags >> libc::MAP_HUGE_SHIFT) & libc::MAP_HUGE_MASK as u64
    {
        0 => default_huge_page,
        shift => 1u64.checked_shl(shift as u32).unwrap_or(u64::MAX),
    };
    match number {
        Sysno::mmap if arg3 & libc::MAP_FIXED as u64 == 0 => false,
        Sysno::mmap if arg3 & libc::MAP_HUGETLB as u64 != 0 => {
            overlaps_huge(arg0, arg1, huge_page(arg3))
        }
        Sysno::mmap => overlaps(arg0, arg1),
        Sysno::mprotect | Sysno::pkey_mprotect | Sysno::madvise | Sysno::munmap => {
            overlaps(arg0, arg1)
        }
        Sysno::mremap => {
            overlaps(arg0, arg1) || (arg3 & libc::MREMAP_FIXED as u64 != 0 && overlaps(arg4, arg2))
        }
        Sysno::shmat => arg1 != 0 && arg2 & libc::SHM_REMAP as u64 != 0 && overlaps(arg1, u64::MAX),
        _ => false,
    }
}

/// Whether an open with `flags` that returned a descriptor may write a
/// process's memory: `/proc/<pid>/mem` and `/proc/<pid>/task/<tid>/mem`
/// write through page protections. `descriptor` is what the descriptor is,
/// read through the opening thread's `/proc/<tid>/fd/<fd>` (see
/// [`descriptor_identity`]): its link, which names the file however the guest
/// reached it (a symlink, a `/proc/self/fd` reopen), and whether it is on
/// procfs. It counts only when it is a procfs file named `mem`, so an
/// ordinary file of that name, or a name that leads to a pipe, does not. A
/// descriptor that cannot be read at all counts, since Detcore cannot
/// establish what it is.
pub(crate) fn may_write_process_memory(flags: OFlag, descriptor: Option<(&Path, bool)>) -> bool {
    let writes = !flags.contains(OFlag::O_PATH) && flags & OFlag::O_ACCMODE != OFlag::O_RDONLY;
    writes
        && descriptor.is_none_or(|(link, on_procfs)| {
            on_procfs && link.file_name() == Some(std::ffi::OsStr::new("mem"))
        })
}

/// The guest call after which [`Detcore::refresh_description_locks`] reads a
/// descriptor's locks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LockCall {
    /// `F_OFD_SETLK` or `F_OFD_SETLKW`.
    Fcntl,
    /// `flock`.
    Flock,
    /// `pidfd_getfd`, which adds a slot.
    PidfdGetfd,
    /// `recvmsg` with `SCM_RIGHTS`, which adds a slot.
    Recvmsg,
}

impl LockCall {
    /// The call's name, as a refusal names it.
    fn name(self) -> &'static str {
        match self {
            Self::Fcntl => "fcntl",
            Self::Flock => "flock",
            Self::PidfdGetfd => "pidfd_getfd",
            Self::Recvmsg => "recvmsg",
        }
    }

    /// Whether the call can take or release a lock.
    fn changes_locks(self) -> bool {
        matches!(self, Self::Fcntl | Self::Flock)
    }
}

/// The inode number an fdinfo names (its `ino:` line), as `/proc/locks`
/// prints the last field of a lock's object.
fn fdinfo_inode(fdinfo: &[u8]) -> Option<String> {
    String::from_utf8_lossy(fdinfo)
        .lines()
        .find_map(|line| line.strip_prefix("ino:"))
        .map(|inode| inode.trim().to_string())
        .filter(|inode| !inode.is_empty() && inode.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The fdinfo page [`Detcore::read_own_fdinfo`] stages its calls in, and its
/// layout: the path, `openat2`'s `struct open_how`, the `fstatfs`/`fstat`
/// buffer, then the read buffer.
const FDINFO_PAGE: usize = 4096;
const FDINFO_PATH: usize = 0;
const FDINFO_HOW: usize = 64;
const FDINFO_HOW_SIZE: usize = 24;
const FDINFO_STAT: usize = 128;
const FDINFO_READ: usize = 512;
const _: () = assert!(FDINFO_HOW + FDINFO_HOW_SIZE <= FDINFO_STAT);
const _: () = assert!(FDINFO_STAT + std::mem::size_of::<libc::stat>() <= FDINFO_READ);
const _: () = assert!(FDINFO_STAT + std::mem::size_of::<libc::statfs>() <= FDINFO_READ);
/// `openat2`'s resolution for the fdinfo entry: no step out of the anchor
/// and no mount crossed.
const FDINFO_RESOLVE: u64 = libc::RESOLVE_NO_XDEV | libc::RESOLVE_BENEATH;

/// What [`Detcore::read_own_fdinfo`] found.
pub(crate) enum FdinfoRead {
    /// The fdinfo's text, from an entry proved to be on the guest's procfs.
    Contents(Vec<u8>),
    /// A step failed for a reason that says nothing about the description's
    /// locks: they are unknown (`procfs::locks::LockLedger::note_unknown`).
    Unknown(String),
    /// The guest's `/proc` could not be proved to be procfs, or a step failed
    /// for a cause the guest's own view or the backend decides
    /// ([`fdinfo_failure_hides`]): the description contributes no lock, which
    /// can hide the guest's row but never admits a host's.
    Untrackable(String),
}

impl FdinfoRead {
    /// The inode number the fdinfo names, if it was read.
    fn inode(&self) -> Option<String> {
        match self {
            Self::Contents(contents) => fdinfo_inode(contents),
            Self::Unknown(_) | Self::Untrackable(_) => None,
        }
    }
}

/// Why [`Detcore::read_staged_fdinfo`] did not read an fdinfo.
enum FdinfoFailure {
    /// A step (`open`, `fstatfs`, `fstat`, `read`) failed with an errno.
    Failed(&'static str, Errno),
    /// The guest's `/proc` was shown not to be procfs, for this reason.
    NotProcfs(&'static str),
}

/// Whether an fdinfo step that failed with `errno` shows the description
/// is untrackable: a cause the guest's own filesystem view or the backend
/// decides, the same in every run of the same program (no `/proc` or an
/// entry missing, ENOENT or ENOTDIR; a symlink loop, ELOOP; an entry outside
/// the anchor or across a mount, EXDEV; a backend that refuses the call,
/// EINVAL, as KVM refuses an `O_PATH` open of its `/proc`). Every other
/// errno, ambient or resource state that can differ between runs, leaves the
/// locks unknown, so a later `/proc/locks` read refuses rather than shows a
/// row that depends on host state: a full descriptor table, EMFILE; a full
/// host file table, ENFILE; no memory, ENOMEM; an interruption, EINTR; an I/O
/// error, EIO; EAGAIN; a kernel without `openat2`, ENOSYS; a permission
/// refusal, EACCES or EPERM, which a host security module keyed by the host
/// process can give as well as the guest's own seccomp filter; anything
/// unlisted.
fn fdinfo_failure_hides(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP | Errno::EXDEV | Errno::EINVAL
    )
}

/// Why Detcore does not know which locks the description descriptor `fd`
/// names holds after the guest's `call` through it: `step` (the open or a
/// read of its fdinfo) failed with `errno`. A failure says nothing about
/// which locks it holds (`Detcore::read_own_fdinfo`); a later `/proc/locks`
/// read refuses with this reason (`procfs::locks::LockLedger::unknown`).
fn fdinfo_refusal(call: &str, fd: RawFd, step: &str, errno: Errno) -> String {
    format!(
        "after {call} on fd {fd}, Detcore could not {step} that descriptor's fdinfo \
         ({errno}), so it does not know which locks the description holds"
    )
}

#[cfg(test)]
mod fdinfo_refusal_tests {
    use super::*;

    /// A guest at its descriptor limit gets EMFILE from Detcore's fdinfo
    /// open after a lock call that needed no new descriptor; a later
    /// `/proc/locks` read stops the run with that named, naming the call,
    /// rather than taking the description to hold nothing. hermit-cli's
    /// `a_full_descriptor_table_refuses_the_next_proc_locks_read` runs that
    /// end to end; this pins the reason's wording.
    #[test]
    fn a_failed_fdinfo_read_is_a_named_refusal_not_an_empty_one() {
        let reason = fdinfo_refusal("recvmsg", 7, "open", Errno::EMFILE);
        assert!(reason.contains("after recvmsg on fd 7"), "{reason}");
        assert!(
            reason.contains("could not open that descriptor's fdinfo"),
            "{reason}"
        );
        assert!(reason.contains("EMFILE"), "{reason}");
        assert!(fdinfo_refusal("flock", 7, "read", Errno::EIO).contains("could not read that"));
    }

    /// A failure the guest's filesystem view or the backend decides hides
    /// the description; ambient or resource state, and anything unlisted,
    /// leaves its locks unknown (and a later `/proc/locks` read refused).
    #[test]
    fn only_a_guest_or_backend_decided_failure_hides_the_description() {
        for errno in [
            Errno::ENOENT,
            Errno::ENOTDIR,
            Errno::ELOOP,
            Errno::EXDEV,
            Errno::EINVAL,
        ] {
            assert!(fdinfo_failure_hides(errno), "{errno}");
        }
        for errno in [
            Errno::EACCES,
            Errno::EPERM,
            Errno::EMFILE,
            Errno::ENFILE,
            Errno::ENOMEM,
            Errno::EINTR,
            Errno::EIO,
            Errno::EAGAIN,
            Errno::ENOSYS,
            Errno::EBADF,
        ] {
            assert!(!fdinfo_failure_hides(errno), "{errno}");
        }
    }

    #[test]
    fn an_fdinfo_names_its_inode_as_proc_locks_prints_it() {
        let fdinfo = b"pos:\t0\nflags:\t0100002\nmnt_id:\t31\nino:\t1234\n\
            lock:\t1: OFDLCK ADVISORY READ -1 00:18:1234 0 0\n";
        assert_eq!(fdinfo_inode(fdinfo).as_deref(), Some("1234"));
        assert_eq!(fdinfo_inode(b"pos:\t0\n"), None);
        assert_eq!(fdinfo_inode(b"ino:\tx\n"), None);
    }
}

/// Writes back the guest bytes that a pre-call lookup buffer (utimensat's, or
/// the removal lookup's) covered.
fn restore_lookup_buffer<M: MemoryAccess>(memory: &mut M, buffer: StatPtr, saved: &[u8]) {
    if memory.write_exact(buffer.0.cast(), saved).is_err() {
        info!("Could not restore the guest bytes under a pre-call lookup buffer.");
    }
}

/// Whether a lookup `buffer` overlaps `path` through its NUL, which a guest
/// call still has to read. A path that cannot be read counts as overlapping,
/// so that the kernel, not a lookup, reports the fault.
fn path_overlaps_buffer<M: MemoryAccess>(
    memory: &M,
    path: Option<syscalls::PathPtr<'_>>,
    buffer: StatPtr,
) -> bool {
    path_overlaps_bytes(
        memory,
        path,
        buffer.0.as_raw(),
        std::mem::size_of::<libc::stat>(),
    )
}

/// Whether the `len` bytes at `start` overlap `path` through its NUL. A path
/// that cannot be read counts as overlapping.
fn path_overlaps_bytes<M: MemoryAccess>(
    memory: &M,
    path: Option<syscalls::PathPtr<'_>>,
    start: usize,
    len: usize,
) -> bool {
    use reverie::syscalls::FromToRaw;

    let end = start.saturating_add(len);
    path.is_some_and(|ptr| match ptr.read(memory) {
        Ok(read) => {
            let addr = Some(ptr).into_raw();
            let read: PathBuf = read;
            addr < end && start < addr.saturating_add(read.as_os_str().len() + 1)
        }
        Err(_) => true,
    })
}

/// Whether the utimensat lookup buffer overlaps the guest memory Linux reads
/// for `call`: the path through its NUL and, with `guest_times`, the two
/// timespecs. A path that cannot be read counts as overlapping, so that the
/// kernel, not a lookup, reports the fault.
fn utimensat_input_overlaps<M: MemoryAccess>(
    memory: &M,
    call: &syscalls::Utimensat,
    guest_times: bool,
    buffer: StatPtr,
) -> bool {
    let start = buffer.0.as_raw();
    let end = start + std::mem::size_of::<libc::stat>();
    let overlaps = |addr: usize, len: usize| addr < end && start < addr.saturating_add(len);
    let path = path_overlaps_buffer(memory, call.path(), buffer);
    let times = guest_times
        && call
            .times()
            .is_some_and(|times| overlaps(times.as_raw(), std::mem::size_of::<[Timespec; 2]>()));
    path || times
}

/// Whether a `*at` stat call names no path: it passes `AT_EMPTY_PATH` with an
/// empty or NULL path (Linux 6.11 accepts NULL), and so describes `dirfd`
/// itself, or the working directory for `AT_FDCWD`. A path that cannot be
/// read counts as nonempty, so the call keeps the path-based numbering and
/// the kernel reports the fault. The path is read only after the flag check,
/// so an ordinary `stat()` reads no guest memory here.
fn names_no_path<M: MemoryAccess>(memory: &M, path: Option<PathPtr>, flags: AtFlags) -> bool {
    flags.contains(AtFlags::AT_EMPTY_PATH)
        && match path {
            None => true,
            Some(path) => path
                .read(memory)
                .is_ok_and(|path| path.as_os_str().is_empty()),
        }
}

/// The fixed inode that `fd` reports when it is still the container's
/// inherited stdin, stdout or stderr, as fdinfo and `/proc/self/fd/N` report
/// it. Every stat call that describes the descriptor itself must use it, or
/// the guest's libc decides which number `fstat()` sees.
fn stdio_inode_override<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
    fd: i32,
) -> Option<DetInode> {
    guest
        .thread_state()
        .with_detfd(fd, |detfd| {
            deterministic_stdio_inode_for_resource(fd, detfd.resource())
        })
        .ok()
        .flatten()
}

/// The file a stat call describes, as Detcore finds it again to learn the
/// canonical path of a procfs file (see [`procfs_inode`]).
#[derive(Clone, Copy)]
enum StatTarget<'a> {
    /// The file open as this descriptor.
    Descriptor(RawFd),
    /// The calling thread's working directory.
    Cwd,
    /// The file `path` names relative to `dirfd`, or the symbolic link itself
    /// when `follow` is false.
    Path {
        dirfd: i32,
        path: Option<PathPtr<'a>>,
        follow: bool,
    },
    /// A file named by a path that the call's result overwrote, which Detcore
    /// cannot look up again.
    Overwritten,
}

impl<'a> StatTarget<'a> {
    /// The target of an `fstatat` or `statx` call with these operands.
    fn at<M: MemoryAccess>(
        memory: &M,
        dirfd: i32,
        path: Option<PathPtr<'a>>,
        flags: AtFlags,
    ) -> Self {
        if names_no_path(memory, path, flags) {
            if dirfd == libc::AT_FDCWD {
                return StatTarget::Cwd;
            }
            if dirfd >= 0 {
                return StatTarget::Descriptor(dirfd);
            }
        }
        StatTarget::Path {
            dirfd,
            path,
            follow: !flags.contains(AtFlags::AT_SYMLINK_NOFOLLOW),
        }
    }

    /// This target, or [`StatTarget::Overwritten`] if it is a path that the
    /// `len` bytes of the call's result at `result` overlap. Linux reads the
    /// path before it writes the result, so the call succeeds, but the path
    /// the guest named is gone once it returns. Found before the call.
    fn unless_overwritten<M: MemoryAccess>(self, memory: &M, result: usize, len: usize) -> Self {
        match self {
            StatTarget::Path { path, .. } if path_overlaps_bytes(memory, path, result, len) => {
                StatTarget::Overwritten
            }
            target => target,
        }
    }

    /// The descriptor the call describes itself, if it names no path.
    fn descriptor(&self) -> Option<RawFd> {
        match self {
            StatTarget::Descriptor(fd) => Some(*fd),
            StatTarget::Cwd | StatTarget::Path { .. } | StatTarget::Overwritten => None,
        }
    }
}

/// The rendered procfs file that a stat result describes, if it is one: by
/// the name Detcore gave it, or by its kernel identity where Detcore names no
/// procfs files. Never a per-process file, whose kernel-reached instances
/// keep the kernel's identity. `files` are the files the session renders
/// (`Detcore::rendered_files`).
fn rendered_entry(
    procfs: &ProcfsEntry,
    stat: &DetStat,
    cfg: &crate::config::Config,
    files: crate::procfs::RenderedFiles,
) -> Option<&'static Path> {
    if cfg.recordreplay_modes {
        return None;
    }
    let found = match procfs {
        ProcfsEntry::At(within) => rendered_procfs::rendered_path_within(within, files),
        ProcfsEntry::NotProcfs | ProcfsEntry::Unknown => {
            rendered_procfs::rendered_path_of_raw(stat, files)
        }
        ProcfsEntry::Unnamed => None,
    };
    // A per-process file the kernel reached (through `..`, after `chroot`,
    // in a nested PID namespace) is served from the kernel's bytes
    // (`ProcfsFile::served_rendered_path`), so it keeps the kernel's metadata
    // too: its identity is the kernel's file's, not the rendered file's.
    found.filter(|path| !crate::procfs::rendered_path_is_per_process(path))
}

/// `STATX_MNT_ID_UNIQUE` (include/uapi/linux/stat.h, since Linux 6.8). Reverie's
/// `StatxMask` does not name it and drops it from a mask read through it, so
/// Detcore reads a call's raw mask ([`statx_requested_mask`]).
const STATX_MNT_ID_UNIQUE: u32 = 0x4000;

/// What Linux adds to every unique mount ID so that none equals a classic
/// 32-bit one: `MNT_UNIQUE_ID_OFFSET` (v6.12 fs/namespace.c:73). Linux counts
/// up from it and never reuses an ID, so the first mount's is the offset plus
/// one (fs/namespace.c:74 and 244).
const MNT_UNIQUE_ID_OFFSET: u64 = 1 << 31;

/// The mask a `statx` call asks for, every bit kept.
fn statx_requested_mask(call: syscalls::Statx) -> u32 {
    syscalls::SyscallArgs::from(call).arg3 as u32
}

/// `call` asking the kernel for the classic mount ID (`STATX_MNT_ID`) where
/// it asked for the unique one, and otherwise the same call.
fn statx_asking_classic_mount_id(call: syscalls::Statx) -> syscalls::Statx {
    let mut args = syscalls::SyscallArgs::from(call);
    let mask = args.arg3 as u32;
    if mask & STATX_MNT_ID_UNIQUE != 0 {
        args.arg3 =
            ((mask & !STATX_MNT_ID_UNIQUE) | reverie::syscalls::linux_abi::STATX_MNT_ID) as usize;
    }
    syscalls::Statx::from(args)
}

/// Whether `statx` accepts these `flags` and `mask` (`vfs_statx`'s and the
/// syscall's own checks): no reserved mask bit, known flags only (unless the
/// call names no path), and not both synchronization types at once.
fn statx_arguments_valid(flags: i32, mask: u32, names_no_path: bool) -> bool {
    let known = libc::AT_SYMLINK_NOFOLLOW
        | libc::AT_EMPTY_PATH
        | libc::AT_NO_AUTOMOUNT
        | reverie::syscalls::linux_abi::AT_STATX_SYNC_TYPE;
    // A call on the descriptor itself (`AT_EMPTY_PATH` with an empty or
    // NULL path) does not look at the other flags (measured on Linux 7.1).
    mask & 0x8000_0000 == 0
        && (names_no_path || flags & !known == 0)
        && flags & reverie::syscalls::linux_abi::AT_STATX_SYNC_TYPE
            != reverie::syscalls::linux_abi::AT_STATX_SYNC_TYPE
}

/// Whether `path` spells a descriptor link: `/dev/fd/N`, `/proc/*/fd/N` or
/// `/proc/*/task/*/fd/N`.
fn names_descriptor_link(path: &Path) -> bool {
    let parts = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => {
                Some(std::os::unix::ffi::OsStrExt::as_bytes(part))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let number = |part: &[u8]| !part.is_empty() && part.iter().all(u8::is_ascii_digit);
    match parts.as_slice() {
        [b"dev", b"fd", fd] | [b"proc", _, b"fd", fd] | [b"proc", _, b"task", _, b"fd", fd] => {
            number(fd)
        }
        _ => false,
    }
}

/// Who opens a path that may be one of its descriptor links, for
/// [`path_to_record_for_open`].
#[derive(Debug, Clone, Copy)]
struct DescriptorLinkOpener {
    /// The caller's pid and tid as the guest's `/proc/<pid>` and
    /// `.../task/<tid>` spellings name them on every backend, or `None`
    /// where Detcore does not know those names (a nested PID namespace).
    /// See [`Detcore::procfs_caller_ids`].
    own: Option<(i32, i32)>,
    /// Whether the descriptor table procfs reads for a process-level link
    /// (`/proc/self/fd/N`, `/dev/fd/N`), the thread-group leader's, is
    /// provably the caller's own: the caller leads its group, or holds the
    /// table the leader holds now (`ThreadState::process_files`). A thread
    /// cloned without `CLONE_FILES` holds a table of its own.
    holds_process_table: bool,
}

/// The descriptor `N` that a descriptor link in an open's spelling names,
/// as it stood when the open looked the link up: in the open's own turn,
/// before the call, which may block and leave the turn (a FIFO waiting for
/// its other end) while a thread sharing the table replaces `N`. The kernel
/// follows the link at that lookup, so the file it opens is the one `N` was
/// then, whatever `N` is when the open completes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkSource {
    fd: RawFd,
    /// The path Detcore had recorded for `N`.
    path: Option<PathBuf>,
    /// `N`'s open file description.
    open_file: OpenFileId,
    /// Where `N`'s `/proc/self` or `/proc/thread-self` spelling led, and the
    /// identity of the task it led to (`DetFd::procfs_reached`).
    reached: Option<(PathBuf, Option<crate::procfs::TaskIncarnation>)>,
}

impl LinkSource {
    /// The path to record, for [`path_to_record_for_open`], for descriptor
    /// `fd` of the caller's table, whose open file description is `current`
    /// when the open completes: `N`'s path as the lookup saw it, only if
    /// `fd` is `N` and `N` still holds that description. Where `N` was
    /// replaced meanwhile, nothing, and the open keeps its spelling, as
    /// before this change.
    fn path_if_unchanged(&self, fd: RawFd, current: Option<OpenFileId>) -> Option<PathBuf> {
        (fd == self.fd && current == Some(self.open_file))
            .then(|| self.path.clone())
            .flatten()
    }

    /// Where `N`'s selector spelling led, for a new descriptor opened through
    /// the link to `N` (`fd`), only if `fd` is `N` and `N` still holds its
    /// description: the new open file is that task's directory too.
    fn reached_if_unchanged(
        &self,
        fd: RawFd,
        current: Option<OpenFileId>,
    ) -> Option<(PathBuf, Option<crate::procfs::TaskIncarnation>)> {
        (fd == self.fd && current == Some(self.open_file))
            .then(|| self.reached.clone())
            .flatten()
    }
}

/// Where the spelling `recorded` led, if it names a task's directory: through
/// a `/proc` selector, `/proc/self`, `/proc/self/task`,
/// `/proc/self/task/<tid>` or `/proc/thread-self`, with the selector replaced
/// by the opener's numbers (`own`, its pid and tid as `/proc` spells them), as
/// Linux resolved it at the open; or by number, `/proc/<pid>`,
/// `/proc/<pid>/task` or `/proc/<pid>/task/<tid>`, itself
/// (`DetFd::procfs_reached`). Linux binds the directory to the task it named
/// at the open, not to the number, so either is that task while it lives.
/// Nothing for any other path, such as a descriptor link (`fd/N`), which
/// leads to another file, and nothing where Detcore does not know the
/// caller's numbers (`own` is `None` in a nested PID namespace).
fn procfs_selector_reached(recorded: &Path, own: Option<(i32, i32)>) -> Option<PathBuf> {
    let (pid, tid) = own?;
    let below = recorded.strip_prefix("/proc").ok()?;
    let names = below
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let number = |name: &str| {
        (!name.starts_with('0') && !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()))
            .then(|| name.parse::<i32>().ok())
            .flatten()
    };
    Some(PathBuf::from(match names.as_slice() {
        ["self"] => format!("/proc/{pid}"),
        ["self", "task"] => format!("/proc/{pid}/task"),
        ["self", "task", thread] => format!("/proc/{pid}/task/{}", number(thread)?),
        ["thread-self"] => format!("/proc/{pid}/task/{tid}"),
        [process] => format!("/proc/{}", number(process)?),
        [process, "task"] => format!("/proc/{}/task", number(process)?),
        [process, "task", thread] => {
            format!("/proc/{}/task/{}", number(process)?, number(thread)?)
        }
        _ => return None,
    }))
}

/// The task whose directory `reached` (where a `/proc` selector led,
/// `procfs_selector_reached`) is or is below: `/proc/<pid>`, and the thread of
/// `/proc/<pid>/task/<tid>`.
fn procfs_reached_task(reached: &Path) -> Option<crate::procfs::BoundTask> {
    let names = reached
        .strip_prefix("/proc")
        .ok()?
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    let number = |name: &str| name.parse::<i32>().ok().filter(|number| *number > 0);
    let process = number(names.first()?)?;
    let thread = match names.as_slice() {
        [_, "task", thread, ..] => Some(number(thread)?),
        _ => None,
    };
    Some(crate::procfs::BoundTask { process, thread })
}

/// Whether `spelling` names a `/proc` selector link itself: `/proc/self` or
/// `/proc/thread-self`, which are symbolic links to a task's directory.
fn names_a_selector_link(spelling: &Path) -> bool {
    spelling.parent() == Some(Path::new("/proc"))
        && spelling
            .file_name()
            .is_some_and(|name| name == "self" || name == "thread-self")
}

/// Whether an open of the absolute spelling `observed` with `flags` opens a
/// descriptor link itself rather than the file it reaches: `O_NOFOLLOW`
/// does not follow the last component (with `O_PATH` the link is opened,
/// without it the open fails with `ELOOP`), unless the spelling follows it
/// anyway with a trailing slash or `/.`.
fn opens_the_link_itself(observed: &Path, flags: OFlag) -> bool {
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(observed.as_os_str());
    flags.contains(OFlag::O_NOFOLLOW) && !(bytes.ends_with(b"/") || bytes.ends_with(b"/."))
}

/// The path Detcore records for a descriptor the guest opened at the absolute
/// spelling `observed` with `flags`. Opened through one of the caller's own
/// descriptor links (`/proc/self/fd/N`, `/dev/fd/N`, ... see
/// [`crate::syscalls::namespace::own_descriptor_link`]), the new descriptor
/// is the file that descriptor `N` is, so it takes the path recorded for `N`
/// (by `recorded`): a name relative to the new descriptor is then looked up
/// below that file, as Linux looks it up, and not below the link's spelling,
/// which names no procfs file of its own.
///
/// It keeps the spelling, as every other open does, wherever the recorded
/// path could name another file than the one the kernel opened:
/// - the open does not follow the link (`O_NOFOLLOW` without a trailing
///   slash or `/.`), so it opened the link itself;
/// - the link names the process's table, which is the thread-group
///   leader's, and the caller is not proven to hold that table
///   (`opener.holds_process_table`); `recorded` reads the caller's;
/// - descriptor `N` has no recorded path, or a relative one;
/// - `N`'s recorded path is resolved per caller (`/proc/self`,
///   `/proc/thread-self`, `/dev/fd`, ... see
///   [`crate::syscalls::namespace::names_through_caller_selector`]): `N` is
///   the file its opener reached, which another process or thread that
///   inherited or shares it would not reach by that path.
///
/// Where the link names the caller's own table and `N` has no recorded path
/// at all, as for a descriptor received through `SCM_RIGHTS`, `identity` is
/// recorded instead if given: the procfs directory that the kernel names for
/// the opened descriptor (see `Detcore::procfs_directory_named_for`). Not
/// where `N`'s recorded path is refused above, nor where the link names a
/// table the caller is not proven to hold, whose `N` is not the one
/// `recorded` describes.
fn path_to_record_for_open(
    observed: &Path,
    flags: OFlag,
    opener: DescriptorLinkOpener,
    recorded: impl FnOnce(RawFd) -> Option<PathBuf>,
    identity: Option<PathBuf>,
) -> PathBuf {
    use crate::syscalls::namespace::DescriptorTable;
    if opens_the_link_itself(observed, flags) {
        return observed.to_path_buf();
    }
    let Some(link) = crate::syscalls::namespace::own_descriptor_link(observed, opener.own) else {
        return observed.to_path_buf();
    };
    if !(link.table == DescriptorTable::Thread || opener.holds_process_table) {
        return observed.to_path_buf();
    }
    match recorded(link.fd) {
        Some(path) => {
            (!crate::syscalls::namespace::names_through_caller_selector(&path)).then_some(path)
        }
        None => identity,
    }
    .unwrap_or_else(|| observed.to_path_buf())
}

/// The components below `/proc` of the kernel's name for a procfs directory
/// (a descriptor link), and the positions among them of the task numbers it
/// names: `/proc/<pid>` and its `task/<tid>`. `None` for a name outside
/// `/proc`, a deleted or anonymous object, a relative name, or a task number
/// procfs would not have (a leading zero).
fn procfs_directory_task_positions(link: &str) -> Option<(Vec<String>, Vec<usize>)> {
    if link.ends_with(" (deleted)") {
        return None;
    }
    let below = link.strip_prefix("/proc")?;
    if !(below.is_empty() || below.starts_with('/')) {
        return None;
    }
    let names = below
        .split('/')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if names.iter().any(|name| name == "." || name == "..") {
        return None;
    }
    let number = |name: &str| {
        !name.is_empty() && !name.starts_with('0') && name.bytes().all(|byte| byte.is_ascii_digit())
    };
    let mut positions = Vec::new();
    if let Some(first) = names.first()
        && first.bytes().all(|byte| byte.is_ascii_digit())
    {
        if !number(first) {
            return None;
        }
        positions.push(0);
        if names.get(1).map(String::as_str) == Some("task")
            && let Some(thread) = names.get(2)
        {
            if !number(thread) {
                return None;
            }
            positions.push(2);
        }
    }
    Some((names, positions))
}

/// Whether `link`, the kernel's name for a descriptor (its
/// `/proc/thread-self/fd/N` link), names a procfs directory path Detcore
/// may record for it: `/proc`, or a path below it that names no process or
/// thread (`procfs::ids::named_ids`, and no numeric component at all),
/// whose numbers are the kernel's and need not be the guest's, nor a
/// selector procfs resolves per caller. Not a deleted directory
/// (`" (deleted)"`), an anonymous object or a relative target, nor any other
/// directory.
fn procfs_directory_link(link: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = link.as_os_str().as_bytes();
    link.starts_with("/proc")
        && !bytes.ends_with(b" (deleted)")
        && link.components().all(|component| match component {
            std::path::Component::RootDir => true,
            std::path::Component::Normal(name) => !name.as_bytes().iter().all(u8::is_ascii_digit),
            _ => false,
        })
        && crate::procfs::ids::named_ids(link).is_empty()
        && !crate::syscalls::namespace::names_through_caller_selector(link)
}

/// Whether a stat call's own arguments are ones Linux accepts before it looks
/// up any file: `newfstatat` refuses unknown flags (`vfs_fstatat`), except on
/// the descriptor itself (`AT_EMPTY_PATH` with an empty path), which it
/// stats without looking at them (measured on Linux 7.1).
fn stat_family_arguments_valid<M: MemoryAccess>(memory: &M, call: &StatFamily) -> bool {
    match call {
        StatFamily::Fstatat(call) => {
            let known = libc::AT_SYMLINK_NOFOLLOW
                | libc::AT_NO_AUTOMOUNT
                | libc::AT_EMPTY_PATH
                | reverie::syscalls::linux_abi::AT_STATX_SYNC_TYPE;
            names_no_path(memory, call.path(), call.flags()) || call.flags().bits() & !known == 0
        }
        _ => true,
    }
}

/// The file a call of the stat family describes.
fn stat_family_target<'a, M: MemoryAccess>(memory: &M, call: &'a StatFamily) -> StatTarget<'a> {
    match call {
        StatFamily::Fstat(call) => StatTarget::Descriptor(call.fd()),
        StatFamily::Fstatat(call) => {
            StatTarget::at(memory, call.dirfd(), call.path(), call.flags())
        }
        #[cfg(not(target_arch = "aarch64"))]
        StatFamily::Stat(call) => StatTarget::Path {
            dirfd: libc::AT_FDCWD,
            path: call.path(),
            follow: true,
        },
        #[cfg(not(target_arch = "aarch64"))]
        StatFamily::Lstat(call) => StatTarget::Path {
            dirfd: libc::AT_FDCWD,
            path: call.path(),
            follow: false,
        },
    }
}

/// How many times Detcore injects the `openat` or the `close` of the
/// `O_PATH` descriptor it opens in the guest to name a procfs entry when the
/// call had no effect, before it records a determinism loss instead.
const OBJECT_DESCRIPTOR_ATTEMPTS: usize = 8;

/// What Detcore learned of a stat result that may describe a procfs file
/// (see [`procfs_inode`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcfsEntry {
    /// Not a file on a procfs mount, or a mode in which Detcore does not name
    /// procfs files.
    NotProcfs,
    /// A procfs file at this path within procfs.
    At(PathBuf),
    /// A procfs file that Detcore could not name. It recorded a determinism
    /// loss.
    Unnamed,
    /// A file Detcore could not tell from a procfs file, as it could not read
    /// the guest's mount table or the file's filesystem type, or did not look,
    /// as the guest's threads are not sequentialized. It recorded a
    /// determinism loss.
    Unknown,
}

impl ProcfsEntry {
    /// Whether the file is known to be on a procfs mount.
    fn is_procfs(&self) -> bool {
        matches!(self, ProcfsEntry::At(_) | ProcfsEntry::Unnamed)
    }

    /// The name of the file, which is on `device`.
    fn name(&self, device: u64) -> Option<ProcfsName> {
        match self {
            ProcfsEntry::At(within) => Some(ProcfsName::new(device, within)),
            ProcfsEntry::NotProcfs | ProcfsEntry::Unnamed | ProcfsEntry::Unknown => None,
        }
    }
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
        // The kernel's number for the guest's (DBT; `procfs_numbers`): one
        // `/proc` does not list has no file to bind.
        let Some(target) = Self::kernel_procfs_number(guest, target).await else {
            return Ok(None);
        };
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

    /// A vectored read of a procfs file Detcore renders: one read of its
    /// snapshot, as Linux's `read_iter` serves the whole vector at once,
    /// scattered over the `count` vectors at `address`, at `offset` or at
    /// (and advancing) the shared cursor. The order is `vfs_readv`'s: the
    /// descriptor's access mode, then the vectors' import (`EFAULT` for any
    /// destination outside the user address range, even at end of file), an
    /// empty vector (0), the range (`rw_verify_area`), and the flags.
    async fn readv_rendered_procfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        address: usize,
        count: usize,
        offset: Option<i64>,
        flags: i32,
    ) -> Result<i64, Error> {
        let status = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.status_flags())?;
        if !can_read(status) {
            return Err(Errno::EBADF.into());
        }
        let iovecs = crate::iovecs::import_read_iovecs(&guest.memory(), address, count, || {
            crate::iovecs::UserAddressLimit::from_query(guest.user_address_limit())
        })?;
        let maximum = iovecs.iter().map(|iovec| iovec.len).sum::<usize>();
        if maximum == 0 {
            return Ok(0);
        }
        if let Some(offset) = offset
            && offset.checked_add(maximum as i64).is_none()
        {
            return Err(Errno::EINVAL.into());
        }
        let read_iter = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_has_read_iter())?;
        validate_procfs_vector_flags(flags, read_iter)?;
        // A file the kernel opened takes its snapshot at its first read.
        if guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.procfs_needs_snapshot())?
        {
            self.initialize_procfs_snapshot(guest, syscalls::Read::new().with_fd(fd))
                .await?;
        } else if let Some(offset) = offset
            && guest.thread_state().with_detfd(fd, |detfd| {
                detfd
                    .procfs_position()
                    .is_some_and(|(position, _)| position as i64 != offset)
            })?
        {
            // A read at a position the buffered snapshot does not start at
            // runs a per-process file's `show` again (`traverse`).
            self.require_rendered_task(guest, fd).await?;
        }
        let start = match offset {
            Some(offset) => offset as usize,
            None => {
                self.adopt_rendered_procfs_position(guest, fd).await?;
                guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.procfs_position())?
                    .map_or(0, |(offset, _)| offset)
            }
        };
        let (bytes, faults_on_a_short_copy) = guest.thread_state().with_detfd(fd, |detfd| {
            (
                detfd.take_procfs_at(start, maximum).unwrap_or_default(),
                detfd.procfs_faults_on_a_short_copy(),
            )
        })?;
        // Like `copy_to_iter`, stop at the first byte that cannot be written,
        // leaving later vectors untouched.
        let mut copied = 0;
        for iovec in iovecs {
            if copied == bytes.len() {
                break;
            }
            if iovec.len == 0 {
                continue;
            }
            let end = (copied + iovec.len).min(bytes.len());
            let written = match copy_procfs_output(
                &mut guest.memory(),
                AddrMut::from_raw(iovec.base),
                &bytes[copied..end],
            ) {
                Ok(written) => written,
                Err(Error::Errno(Errno::EFAULT)) => 0,
                Err(error) => return Err(error),
            };
            copied += written;
            if copied < end {
                break;
            }
        }
        if copied < bytes.len() && (faults_on_a_short_copy || copied == 0) {
            return Err(Errno::EFAULT.into());
        }
        if offset.is_none() {
            guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.commit_procfs_read(start, copied))?;
            self.sync_rendered_procfs_position(guest, fd).await?;
        }
        Ok(copied as i64)
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
    /// Set the kernel's O_NONBLOCK on `fd`'s open file description, keeping its
    /// other status flags. The caller records the change with
    /// `maybe_set_nonblocking_fd`; the guest-visible flags are unchanged.
    async fn inject_physical_nonblocking<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<(), Errno> {
        let flags = guest
            .inject(syscalls::Fcntl::new().with_fd(fd).with_cmd(F_GETFL))
            .await?;
        guest
            .inject(
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(F_SETFL(flags as i32 | OFlag::O_NONBLOCK.bits())),
            )
            .await?;
        Ok(())
    }

    /// A blocking `openat` of the FIFO `fifo` (found by the tracer at `path`),
    /// which a guest made in this run (`crate::fifo_peer::is_guest_made`), for
    /// reading or for writing. On Linux it waits in the kernel until the other
    /// end is open; injected as is, it would wait while it holds the scheduler
    /// turn, and a guest that opens the other end could never run
    /// (<https://github.com/rrnewton/hermit/issues/2203>).
    ///
    /// First the open is tried nonblocking. Any error other than "no reader
    /// yet" (`ENXIO`, for a writer) is the open's own, returned at once, as
    /// Linux returns it before waiting (`EEXIST`, `ENOENT`, `EACCES`, ...), and
    /// nothing else is opened. A writer that finds a reader completes then, as
    /// does a reader whose writer a guest holds or waits to open. Otherwise the
    /// open waits in polling turns ([`crate::fifo_peer::PendingOpen`]), and the
    /// guest's try is closed in the same turn, so no sibling sees a descriptor
    /// for it. It waits until a guest holds the other end, is waiting to open
    /// it, or has opened it since, then opens nonblocking and is made
    /// blocking, as the guest asked.
    ///
    /// A signal ends the wait as Linux's: `ERESTARTSYS`, which the kernel turns
    /// into `EINTR` or a restart by the guest's disposition.
    ///
    /// When the scheduler answers that nothing else can run
    /// (`ResumeStatus::NothingElseCanRun`), no guest can open the other end,
    /// so the open is made as the guest asked, waiting in the kernel while it
    /// holds the turn, as without the poll: only a host could complete it.
    ///
    /// Either way the open completes on the FIFO it waited on
    /// ([`Self::open_fifo_waited_on`]).
    async fn open_fifo_once_a_guest_holds_its_peer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
        fifo: (u64, u64),
        path: &std::ffi::OsStr,
    ) -> Result<i64, Error> {
        let reading = call.flags().bits() & libc::O_ACCMODE == libc::O_RDONLY;
        let peer = if reading {
            FifoPeer::Writer
        } else {
            FifoPeer::Reader
        };
        let nonblocking = call.with_flags(call.flags() | OFlag::O_NONBLOCK);
        let opens_before = crate::fifo_peer::guest_opens(fifo);
        // Guest state only, which changes in guest turns: the answer at each
        // polling turn is deterministic.
        let guest_peer_ready = || {
            crate::fifo_peer::guest_holds_peer_of(fifo, peer)
                || crate::fifo_peer::pending_opens(fifo).of(peer) > 0
                || crate::fifo_peer::guest_opens(fifo).of(peer) > opens_before.of(peer)
        };
        // A nonblocking FIFO open never waits, so an injection a signal
        // interrupts did not run, and is made again; the signal is delivered
        // as the guest resumes.
        let pending = match guest.inject_with_retry(nonblocking).await {
            // A writer found a reader: Linux's open completes at once.
            Ok(fd) if !reading => return Ok(self.make_blocking(guest, fd as RawFd).await?),
            Ok(fd) if guest_peer_ready() => {
                crate::fifo_peer::stand_in_for_waiting_writers(fifo, path);
                return Ok(self.make_blocking(guest, fd as RawFd).await?);
            }
            Ok(fd) => {
                // The tracer's reader opens before the guest's try closes, so
                // the FIFO keeps a reader throughout.
                let pending = crate::fifo_peer::PendingOpen::begin(fifo, path, call.flags().bits());
                guest
                    .inject_with_retry(syscalls::Close::new().with_fd(fd as RawFd))
                    .await?;
                pending
            }
            Err(Errno::ENXIO) if !reading => {
                crate::fifo_peer::PendingOpen::begin(fifo, path, call.flags().bits())
            }
            Err(errno) => return Err(errno.into()),
        };
        info!("[detcore] the FIFO open polls for a guest peer");
        // A waiting open is an end on Linux: it readies the opens waiting for
        // this end (`--pipe-wake-latency`).
        let in_effect = self.cfg.pipe_wake_latency_in_effect().is_some();
        let channel = crate::pipe_wake::ChannelId::Fifo(if in_effect {
            crate::fifo_peer::fifo_channel(fifo)
        } else {
            0
        });
        let (readies, waits_for) = if reading {
            (
                crate::pipe_wake::Wants::PeerReader,
                crate::pipe_wake::Wants::PeerWriter,
            )
        } else {
            (
                crate::pipe_wake::Wants::PeerWriter,
                crate::pipe_wake::Wants::PeerReader,
            )
        };
        self.note_pipe_readied(guest, channel, readies).await;

        let mut resources = Resources::new(guest.thread_state().dettid);
        resources.insert(ResourceID::InternalIOPolling, Permission::W);
        resources.fyi(call.name());
        resources.polls_guest_state_only = true;
        resources.waits_until_nothing_else_can_run = true;
        if in_effect {
            resources.polls_channel = Some(Box::new((channel, waits_for)));
        }
        // As a polled pipe read: a signal another task sends ends the wait.
        resources.set_signal_interrupt_errno(Errno::ERESTARTSYS);
        resources.signal_state_task = crate::syscalls::helpers::guest_signal_state_task(guest);
        // The peer is checked again after every answer, the last one included:
        // a guest may have opened (and closed) the other end in the turns
        // before it.
        loop {
            let status = resource_request(guest, resources.clone()).await;
            resources.poll_attempt += 1;
            if guest_peer_ready() {
                break;
            }
            match status {
                ResumeStatus::Signaled(_) => return Err(Errno::ERESTARTSYS.into()),
                ResumeStatus::NothingElseCanRun => {
                    info!(
                        "[detcore] nothing else can run; the FIFO open waits in the kernel, as without the poll"
                    );
                    let waited_on = pending.guest_path();
                    return self
                        .open_fifo_waited_on(
                            guest,
                            call,
                            fifo,
                            waited_on.as_deref(),
                            call.flags(),
                            false,
                        )
                        .await;
                }
                _ => {}
            }
        }
        let waited_on = pending.guest_path();
        let open = async |guest: &mut G| {
            self.open_fifo_waited_on(
                guest,
                call,
                fifo,
                waited_on.as_deref(),
                nonblocking.flags(),
                true,
            )
            .await
        };
        let mut opened = open(guest).await;
        if matches!(opened, Err(Error::Errno(Errno::ENXIO))) {
            // A writer whose only reader has come and gone opens as Linux's did
            // when the reader opened; the tracer stands in for that reader while
            // the guest opens, so the guest's writes then fail with `EPIPE`, as
            // on Linux.
            let _stand_in = pending.open_in_tracer(libc::O_RDONLY);
            opened = open(guest).await;
        }
        if reading && opened.is_ok() {
            pending.stand_in_for_waiting_writers();
        }
        drop(pending);
        Ok(self.make_blocking(guest, opened? as RawFd).await?)
    }

    /// Open, in the guest, the FIFO `fifo` a blocking open waited on, with
    /// the guest's `flags`, whatever its name is now: through the tracer's
    /// descriptor for it, `waited_on` ([`Self::open_waited_on_fifo`]).
    ///
    /// The kernel lets a guest into the tracer's descriptors only if it holds
    /// `CAP_SYS_PTRACE` or every capability the tracer has
    /// (`proc_fd_access_allowed`), and only if the tracer's `/proc` is under
    /// its root; and a root whose `/proc` is not the tracer's may name another
    /// file at that path, so what it opened is checked to be the FIFO, and
    /// closed if not. A guest that cannot reach the descriptor (`EACCES`,
    /// `ENOENT`, another file), or whose open waited without one (`waited_on`
    /// is `None`),
    /// opens the FIFO by the name it gave, as its first try did, provided the
    /// name still reaches that FIFO: looked up first, so that no other file
    /// is opened, and checked again once open. If the name reaches another
    /// file or none, the open cannot complete on its FIFO, as Linux's does,
    /// and is refused (Claude re-check of
    /// <https://github.com/rrnewton/hermit/pull/4329> at revision 4,
    /// MEDIUM-A).
    ///
    /// Either way this is a new open, so the FIFO's mode is checked again
    /// against the guest's credentials when it completes: a FIFO whose mode
    /// the guest may no longer open (a `chmod` during the wait) gives
    /// `EACCES`, where Linux checked only at the start.
    ///
    /// `retry`: an open that cannot wait (nonblocking), which a signal stops
    /// only before it runs. `ENXIO` (a writer whose reader is gone) and
    /// `ERESTARTSYS` (a signal ended a blocking open) are the caller's.
    async fn open_fifo_waited_on<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
        fifo: (u64, u64),
        waited_on: Option<&str>,
        flags: OFlag,
        retry: bool,
    ) -> Result<i64, Error> {
        let is_the_fifo = |stat: &libc::stat| (stat.st_dev, stat.st_ino) == fifo;
        let unreachable = match waited_on {
            Some(waited_on) => {
                match Self::open_waited_on_fifo(guest, fifo, waited_on, flags, retry).await {
                    Err(errno) if !matches!(errno, Errno::ENXIO | Errno::ERESTARTSYS) => {
                        Some(errno)
                    }
                    // A root whose `/proc` is not the tracer's can name
                    // another file at that path (Claude re-check at
                    // revision 5, LOW-C).
                    Ok(fd)
                        if !self
                            .inject_fstat(guest, fd as RawFd)
                            .await
                            .is_ok_and(|stat| is_the_fifo(&stat)) =>
                    {
                        info!(
                            "[detcore] the tracer's descriptor path reached another file in the guest's root"
                        );
                        guest
                            .inject_with_retry(syscalls::Close::new().with_fd(fd as RawFd))
                            .await?;
                        Some(Errno::ENOENT)
                    }
                    opened => return Ok(opened?),
                }
            }
            None => None,
        };
        if let Some(errno) = unreachable {
            info!(
                "[detcore] the guest cannot reach the tracer's descriptor for the FIFO ({}); it opens the FIFO by its name",
                errno
            );
        }
        let path = call.path().ok_or(Errno::EFAULT)?;
        let lookup = if flags.contains(OFlag::O_NOFOLLOW) {
            AtFlags::AT_SYMLINK_NOFOLLOW
        } else {
            AtFlags::empty()
        };
        if Self::inject_statat(guest, call.dirfd(), path, [Some(path), None], lookup)
            .await
            .is_ok_and(|stat| is_the_fifo(&stat))
        {
            // The name was checked by the first try, and reaches the FIFO:
            // nothing is created.
            let open = call.with_flags(flags - (OFlag::O_CREAT | OFlag::O_EXCL));
            let fd = if retry {
                guest.inject_with_retry(open).await?
            } else {
                guest.inject(open).await?
            };
            if self
                .inject_fstat(guest, fd as RawFd)
                .await
                .is_ok_and(|stat| is_the_fifo(&stat))
            {
                return Ok(fd);
            }
            guest
                .inject_with_retry(syscalls::Close::new().with_fd(fd as RawFd))
                .await?;
        }
        error!(
            "[detcore] a blocking open of a FIFO a guest made waited for its peer, and cannot complete \
             on that FIFO: the guest cannot reach the tracer's descriptor for it, and its name no \
             longer reaches it (a guest without CAP_SYS_PTRACE, or without the tracer's /proc, whose \
             FIFO was renamed or unlinked during the wait); refused \
             (https://github.com/rrnewton/hermit/pull/4329)"
        );
        self.refuse_unserviceable_operation(
            guest,
            Sysno::openat,
            unreachable.unwrap_or(Errno::ENOENT),
        )
        .await
    }

    /// Open, in the guest, the FIFO a waiting open waited on, at `waited_on`
    /// (`fifo_peer::PendingOpen::guest_path`), with the guest's `flags`: the
    /// checks of its name were made by the first try, so `O_CREAT`, `O_EXCL`
    /// and `O_NOFOLLOW` go (the path is the tracer's descriptor link).
    /// `retry`: an open that cannot wait (nonblocking), which a signal stops
    /// only before it runs.
    ///
    /// The path is staged in a private page mapped for this open only, not
    /// in the guest stack scratch, so that an open needs no spare stack
    /// below the guest's stack pointer, which Linux's does not (Codex
    /// re-check of <https://github.com/rrnewton/hermit/pull/4329> at revision
    /// 4, P2). The page is unmapped before the guest resumes, so the guest's
    /// memory keeps no tracer descriptor number (Claude's LOW-B there). An
    /// error mapping or writing the page is returned, and the caller opens
    /// the FIFO by its name instead.
    ///
    /// A task killed while the open waits in the kernel drops this call
    /// before its unmap; the page is then unmapped by the next task of the
    /// same address space to reach Detcore
    /// ([`crate::fifo_peer::StagedPage`], [`Self::unmap_orphaned_staging_pages`]).
    ///
    /// What the path names is looked up first, and must be the FIFO `fifo`:
    /// a root whose `/proc` is not the tracer's can name another file there,
    /// which is not opened (`ESTALE`; Claude re-check at revision 6, LOW-H).
    async fn open_waited_on_fifo<G: Guest<Self>>(
        guest: &mut G,
        fifo: (u64, u64),
        waited_on: &str,
        flags: OFlag,
        retry: bool,
    ) -> Result<i64, Errno> {
        let mut path_buffer = [0_u8; crate::fifo_peer::STAGED_PAGE_LENGTH];
        let bytes = waited_on.as_bytes();
        if bytes.len() >= path_buffer.len() {
            return Err(Errno::ENAMETOOLONG);
        }
        path_buffer[..bytes.len()].copy_from_slice(bytes);
        // The page's owner exists before its `mmap` runs, at an address
        // chosen first: a task killed during the `mmap` leaves its address
        // space's next task an address to check.
        let address =
            crate::fifo_peer::free_staging_address(guest.tid().as_raw()).ok_or(Errno::ENOMEM)?;
        let page = AddrMut::<u8>::from_raw(address).ok_or(Errno::ENOMEM)?;
        let staged =
            crate::fifo_peer::StagedPage::new(guest.thread_state().mm_id, address, path_buffer);
        match guest
            .inject_with_retry(Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(Some(page.cast::<libc::c_void>().into()))
                    .with_len(path_buffer.len())
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(
                        MapFlags::MAP_PRIVATE
                            | MapFlags::MAP_ANONYMOUS
                            | MapFlags::MAP_FIXED_NOREPLACE,
                    )
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await
        {
            Ok(mapped) if mapped as usize == address => {
                crate::race_probe::delay(crate::race_probe::FIFO_STAGING_MAPPED).await;
            }
            // Nothing was mapped at the address (`EEXIST`: something is
            // there): this route is not taken.
            Err(errno) => {
                staged.unmapped();
                return Err(errno);
            }
            // A kernel without MAP_FIXED_NOREPLACE takes the address as a
            // hint and maps elsewhere.
            Ok(mapped) => {
                staged.unmapped();
                if let Some(elsewhere) = Addr::<libc::c_void>::from_raw(mapped as usize) {
                    let _ = guest
                        .inject_with_retry(Syscall::Munmap(
                            syscalls::Munmap::new()
                                .with_addr(Some(elsewhere))
                                .with_len(path_buffer.len()),
                        ))
                        .await;
                }
                return Err(Errno::ENOMEM);
            }
        }
        let path = PathPtr::from_ptr(page.as_raw() as *const libc::c_char)
            .expect("a mapped page is not null");
        let opened = match guest.memory().write_exact(page, &path_buffer) {
            Ok(()) => match Self::inject_statat(
                guest,
                libc::AT_FDCWD,
                path,
                [Some(path), None],
                AtFlags::empty(),
            )
            .await
            {
                Ok(stat) if (stat.st_dev, stat.st_ino) == fifo => {
                    let open = syscalls::Openat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(Some(path))
                        .with_flags(flags - (OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW))
                        .with_mode(None);
                    if retry {
                        guest.inject_with_retry(open).await
                    } else {
                        guest.inject(open).await
                    }
                }
                Ok(_) => {
                    info!(
                        "[detcore] the tracer's descriptor path names another file in the guest's root"
                    );
                    Err(Errno::ESTALE)
                }
                Err(errno) => Err(errno),
            },
            Err(error) => Err(error),
        };
        match guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Some(page.cast::<libc::c_void>().into()))
                    .with_len(path_buffer.len()),
            ))
            .await
        {
            Ok(_) => staged.unmapped(),
            // An unmap that did not run (stopped before it ran) is left, with
            // the guard, to this address space's next task.
            Err(errno) => info!(
                "[detcore] the transient FIFO path page is left to the next task: {}",
                errno
            ),
        }
        opened
    }

    /// Unmap the pages a completing FIFO open staged in this task's address
    /// space and could not unmap, because its task went away while the open
    /// waited ([`crate::fifo_peer::StagedPage`]). Called as a task reaches
    /// Detcore, before it runs guest code again; costs one load while no page
    /// is left.
    pub(crate) async fn unmap_orphaned_staging_pages<G: Guest<Self>>(guest: &mut G) {
        // Each taken page is owned by its guard until it is unmapped: one this
        // task does not get to (its unmap does not run, or the task exits
        // while it waits) is left again.
        for page in crate::fifo_peer::take_orphaned_pages(guest.thread_state().mm_id) {
            // Only a page that still holds what was staged: the guest may have
            // unmapped it and mapped something else there.
            let mut contents = [0_u8; crate::fifo_peer::STAGED_PAGE_LENGTH];
            let Some(address) = Addr::<u8>::from_raw(page.address()) else {
                page.unmapped();
                continue;
            };
            let readable = guest.memory().read_exact(address, &mut contents).is_ok();
            // A task killed during the `mmap` may have left the page before
            // anything was written to it: a page of zeros is the staging page
            // only where it is a lone one-page anonymous mapping.
            if !readable
                || (contents != *page.contents()
                    && !(contents == [0; crate::fifo_peer::STAGED_PAGE_LENGTH]
                        && crate::fifo_peer::is_lone_staging_mapping(
                            guest.tid().as_raw(),
                            page.address(),
                        )))
            {
                info!("[detcore] a FIFO open's path page that a killed task left is gone");
                page.unmapped();
                continue;
            }
            info!("[detcore] unmapping a FIFO open's path page that a killed task left");
            crate::race_probe::delay(crate::race_probe::FIFO_STAGING_RECLAIM).await;
            let unmapped = guest
                .inject_with_retry(Syscall::Munmap(
                    syscalls::Munmap::new()
                        .with_addr(Addr::<libc::c_void>::from_raw(page.address()))
                        .with_len(contents.len()),
                ))
                .await;
            match unmapped {
                Ok(_) => page.unmapped(),
                Err(Errno::EINVAL) => {
                    warn!(
                        "[detcore] could not unmap the transient FIFO path page: {}",
                        Errno::EINVAL
                    );
                    page.unmapped();
                }
                // Stopped before it ran: the guard leaves it to the next task
                // (Claude re-check at revision 6, MEDIUM-G).
                Err(errno) => info!(
                    "[detcore] the transient FIFO path page is left to the next task: {}",
                    errno
                ),
            }
        }
    }

    /// Clear `O_NONBLOCK` on the guest's FIFO descriptor `fd`, which was opened
    /// nonblocking in place of a blocking open, and return it.
    async fn make_blocking<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) -> Result<i64, Errno> {
        let flags = guest
            .inject_with_retry(syscalls::Fcntl::new().with_fd(fd).with_cmd(F_GETFL))
            .await?;
        guest
            .inject_with_retry(
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(F_SETFL(flags as i32 & !OFlag::O_NONBLOCK.bits())),
            )
            .await?;
        Ok(fd as i64)
    }

    /// Before a logically blocking read (`peer` is `Writer`) or write
    /// (`Reader`) on a FIFO that `handle_openat` marked: if a guest holds the
    /// other end (`crate::fifo_peer`), shrink the FIFO's capacity to the one
    /// `handle_pipe2` pins a pipe to (never growing one the guest shrank), make
    /// it physically nonblocking, and treat it from then on as a
    /// scheduler-managed pipe. The call then polls in
    /// turns instead of blocking in the kernel while it holds the turn, where
    /// the guest at the other end could never run
    /// (<https://github.com/rrnewton/hermit/issues/4200>). A FIFO whose other
    /// ends are all on the host is left blocking, as before.
    ///
    /// The open file description may be shared with another process that
    /// already made this change: the kernel then reports it nonblocking while
    /// this process still sees it blocking, and this process follows. On
    /// ptrace a forked child shares the parent's model (`fork_for` clones the
    /// `Arc`), so it sees the change at once; this branch is for a backend
    /// whose fork copies the model into the child's address space (DBT), and
    /// the ptrace tests do not reach it.
    /// Shrink a FIFO that is about to be polled as a pipe to the capacity
    /// `handle_pipe2` pins a pipe to, if it is larger, so that its partial
    /// writes do not depend on the host's pipe defaults. A capacity the guest
    /// set smaller is kept, as Linux keeps it. The kernel refuses (`EBUSY`)
    /// while more is buffered than the new capacity holds; the FIFO is then
    /// polled at its current capacity, which only that buffered amount
    /// decides.
    async fn shrink_fifo_capacity<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) {
        let Ok(capacity) = guest
            .inject(syscalls::Fcntl::new().with_fd(fd).with_cmd(F_GETPIPE_SZ))
            .await
        else {
            return;
        };
        if capacity > i64::from(DETERMINISTIC_PIPE_CAPACITY_BYTES)
            && let Err(errno) = guest
                .inject(
                    syscalls::Fcntl::new()
                        .with_fd(fd)
                        .with_cmd(F_SETPIPE_SZ(DETERMINISTIC_PIPE_CAPACITY_BYTES)),
                )
                .await
        {
            info!(
                "[detcore] FIFO fd {} keeps its {}-byte capacity: shrinking it failed with {}",
                fd, capacity, errno
            );
        }
    }

    async fn fifo_polls_if_a_guest_holds_its_peer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        peer: FifoPeer,
    ) {
        let Ok((awaiting, logically_nonblocking)) = guest.thread_state().with_detfd(fd, |detfd| {
            (detfd.fifo_awaiting_guest_peer(), detfd.is_nonblocking())
        }) else {
            return;
        };
        if !awaiting || logically_nonblocking {
            return;
        }
        // The calling thread's own table: it may not be its leader's (a thread
        // cloned without CLONE_FILES), and `fd` is changed in this one.
        let tid = guest.tid().as_raw();
        let joins = match crate::fifo_peer::kernel_nonblocking(tid, fd) {
            Some(true) => true,
            Some(false) => {
                crate::fifo_peer::guest_holds_peer(tid, fd, peer) && {
                    self.shrink_fifo_capacity(guest, fd).await;
                    self.inject_physical_nonblocking(guest, fd).await.is_ok()
                }
            }
            None => false,
        };
        if joins {
            info!(
                "[detcore] fd {} is a FIFO with a guest at its other end; polling it as a pipe",
                fd
            );
            let _ = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.fifo_joins_internal_polling());
        }
    }

    /// Inject an extra fstat to retrieve file metadata.
    ///
    /// The kernel needs a writable `struct stat` in the guest. It is staged
    /// first in the guest stack scratch, which on the ptrace backend lies just
    /// below the red zone under the guest's stack pointer and costs nothing
    /// when the stack has room. That memory is not guaranteed to be writable:
    /// the guest may run with its stack pointer just above a guard page (a
    /// thread, fiber or alternate signal stack), or within a few hundred bytes
    /// of the lowest page of the main-thread stack, which a tracer's write does
    /// not grow. The fault then belongs to Detcore's bookkeeping, not to the
    /// guest, so the same fstat is repeated with its buffer in a transient
    /// private page that is unmapped before the guest resumes
    /// (<https://github.com/rrnewton/hermit/issues/3328>).
    pub(crate) async fn inject_fstat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        raw_fd: RawFd,
    ) -> Result<libc::stat, Errno> {
        crate::ordered_info!(
            "Injecting additional fstat to retrieve file metadata on fd {}.",
            raw_fd
        );
        let fstat = move |statptr: StatPtr<'_>| {
            Syscall::Fstat(
                syscalls::Fstat::new()
                    .with_fd(raw_fd)
                    .with_stat(Some(statptr)),
            )
        };
        let copied = match Self::inject_stat_on_stack(guest, fstat).await {
            Err(Errno::EFAULT) => {
                info!(
                    "Guest stack scratch cannot hold the fstat buffer for fd {}; \
                     using a transient page instead.",
                    raw_fd
                );
                Self::inject_stat_in_transient_page(guest, fstat).await?
            }
            result => result?,
        };
        trace!("extra fstat returned inode {}", copied.st_ino);
        Ok(copied)
    }

    /// `fstatat(dirfd, path, AT_SYMLINK_NOFOLLOW)` for a path the guest named
    /// in a call that has not run yet and must still read `inputs`, its paths
    /// ([`Detcore::inject_statat`]).
    async fn inject_lstatat<G: Guest<Self>>(
        guest: &mut G,
        dirfd: RawFd,
        path: syscalls::PathPtr<'_>,
        inputs: [Option<syscalls::PathPtr<'_>>; 2],
    ) -> Result<libc::stat, Errno> {
        Self::inject_statat(guest, dirfd, path, inputs, AtFlags::AT_SYMLINK_NOFOLLOW).await
    }

    /// `fstatat(dirfd, path, flags)` for a path the guest named in a call that
    /// has not run yet and must still read `inputs`, its paths.
    ///
    /// The buffer is staged as the utimensat target lookup stages its own: in
    /// the guest stack scratch, below the red zone, where a raw syscall may
    /// keep its own path, so only when it overlaps none of `inputs`, and with
    /// the bytes under it saved and put back after the commit and after the
    /// lookup, so that the call reads its inputs unchanged (claude review of
    /// <https://github.com/rrnewton/hermit/pull/3849>, P3-2). Otherwise it is
    /// a transient private page, unmapped before the guest resumes.
    pub(crate) async fn inject_statat<G: Guest<Self>>(
        guest: &mut G,
        dirfd: RawFd,
        path: syscalls::PathPtr<'_>,
        inputs: [Option<syscalls::PathPtr<'_>>; 2],
        flags: AtFlags,
    ) -> Result<libc::stat, Errno> {
        // A `/proc` path naming a process or thread by the guest's number
        // is looked up by the kernel's (DBT), as the guest's own call is
        // (`procfs_numbers`).
        let probe = Syscall::Newfstatat(
            syscalls::Newfstatat::new()
                .with_dirfd(dirfd)
                .with_path(Some(path))
                .with_flags(flags),
        );
        let Some(translated) = Self::procfs_translated_call(guest, &probe).await? else {
            return Self::inject_statat_as_spelled(guest, dirfd, path, inputs, flags).await;
        };
        // The one path is staged at the start of the transient page.
        let respelled = PathPtr::from_ptr(translated.page.as_raw() as *const libc::c_char);
        let looked_up = match respelled {
            Some(respelled) => {
                Self::inject_statat_as_spelled(
                    guest,
                    dirfd,
                    respelled,
                    [Some(respelled), None],
                    flags,
                )
                .await
            }
            None => Err(Errno::ENOMEM),
        };
        Self::finish_translated_call(guest, translated).await;
        looked_up
    }

    /// [`Self::inject_statat`] of `path` as it is spelled.
    async fn inject_statat_as_spelled<G: Guest<Self>>(
        guest: &mut G,
        dirfd: RawFd,
        path: syscalls::PathPtr<'_>,
        inputs: [Option<syscalls::PathPtr<'_>>; 2],
        flags: AtFlags,
    ) -> Result<libc::stat, Errno> {
        let lstatat = move |statptr: StatPtr<'_>| {
            Syscall::Newfstatat(
                syscalls::Newfstatat::new()
                    .with_dirfd(dirfd)
                    .with_path(Some(path))
                    .with_stat(Some(statptr))
                    .with_flags(flags),
            )
        };
        // The scratch starts 128 bytes below the stack pointer, at addresses
        // computed by subtraction.
        let scratch = 128 + std::mem::size_of::<libc::stat>();
        if usize::try_from(guest.regs().await.rsp).map_or(true, |rsp| rsp <= scratch) {
            return Self::inject_stat_in_transient_page(guest, lstatat).await;
        }
        let mut stack = guest.stack().await;
        let statptr: StatPtr = StatPtr(stack.reserve());
        let mut saved = [0u8; std::mem::size_of::<libc::stat>()];
        let staged_on_stack = !inputs
            .iter()
            .any(|input| path_overlaps_buffer(&guest.memory(), *input, statptr))
            && guest
                .memory()
                .read_exact(statptr.0.cast(), &mut saved)
                .is_ok();
        if !staged_on_stack {
            drop(stack);
            return Self::inject_stat_in_transient_page(guest, lstatat).await;
        }
        let guard = match stack.commit() {
            Ok(guard) => guard,
            Err(_) => {
                restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
                return Self::inject_stat_in_transient_page(guest, lstatat).await;
            }
        };
        restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
        let looked_up = Self::inject_stat_into(guest, lstatat(statptr), statptr).await;
        restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
        drop(guard);
        looked_up
    }

    /// The fast path of [`Self::inject_fstat`]: the buffer lives in the guest
    /// stack scratch. Returns `EFAULT` when that scratch is not writable.
    async fn inject_stat_on_stack<G: Guest<Self>>(
        guest: &mut G,
        stat_call: impl Fn(StatPtr<'_>) -> Syscall,
    ) -> Result<libc::stat, Errno> {
        let mut stack = guest.stack().await;
        let statptr: StatPtr = StatPtr(stack.reserve());
        // Ptrace lends bytes below the guest red zone. Save them before
        // commit can zero or partially write the reserved buffer, as the
        // adjacent pre-call lookup path does. Tool-owned scratch is likewise
        // held by stack/guard throughout every access.
        let mut saved = [0u8; std::mem::size_of::<libc::stat>()];
        guest.memory().read_exact(statptr.0.cast(), &mut saved)?;
        let guard = match stack.commit() {
            Ok(guard) => guard,
            Err(errno) => {
                restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
                return Err(errno);
            }
        };
        restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
        // No early `?`: even kernel/readback errors must put back the borrowed
        // bytes before the guard releases a backend's Tool-owned arena.
        let copied = Self::inject_stat_into(guest, stat_call(statptr), statptr).await;
        let restored = guest.memory().write_exact(statptr.0.cast(), &saved);
        drop(guard);
        // Preserve the actual injection/readback errno. On success, preserve
        // the old contract that a failed final scratch write is an error.
        match copied {
            Ok(copied) => restored.map(|()| copied),
            Err(errno) => {
                if restored.is_err() {
                    info!("Could not restore the guest bytes under a pre-call lookup buffer.");
                }
                Err(errno)
            }
        }
    }

    /// The fallback of [`Self::inject_fstat`] and [`Self::inject_lstatat`]:
    /// the buffer lives in a private
    /// anonymous page mapped for this call only. The guest never learns its
    /// address, and it is unmapped before the guest resumes, so the guest's
    /// address space is the same as before the call.
    async fn inject_stat_in_transient_page<G: Guest<Self>>(
        guest: &mut G,
        stat_call: impl Fn(StatPtr<'_>) -> Syscall,
    ) -> Result<libc::stat, Errno> {
        let len = std::mem::size_of::<libc::stat>();
        let mapped = guest
            .inject_with_retry(Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(len)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await?;
        let page = usize::try_from(mapped)
            .ok()
            .and_then(AddrMut::<libc::stat>::from_raw)
            .unwrap_or_else(|| panic!("transient fstat page mmap returned {mapped}"));
        let copied = Self::inject_stat_into(guest, stat_call(StatPtr(page)), StatPtr(page)).await;
        if let Err(errno) = guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Some(page.cast::<libc::c_void>().into()))
                    .with_len(len),
            ))
            .await
        {
            // Not expected: the page was mapped by this call and its address
            // never reached the guest. The metadata is still valid, so a
            // leftover page is no reason to fail the guest's syscall.
            warn!(
                "[detcore] could not unmap the transient stat page: {}",
                errno
            );
        }
        copied
    }

    /// Inject `stat_call`, which writes a `struct stat` at `statptr`, and read
    /// back what the kernel wrote.
    async fn inject_stat_into<G: Guest<Self>>(
        guest: &mut G,
        stat_call: Syscall,
        statptr: StatPtr<'_>,
    ) -> Result<libc::stat, Errno> {
        // NOTE: Must retry the injection here. This could get interrupted and
        // we don't want to rerun the entire syscall handler twice.
        guest.inject_with_retry(stat_call).await?;
        statptr.read(&guest.memory())
    }

    // helper function to track a new file descriptor.
    pub(crate) async fn add_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        flags: OFlag,
        ty: FdType,
    ) -> Result<(), Errno> {
        self.add_fd_with_stat(guest, fd, flags, ty).await.map(drop)
    }

    /// Track `fd`, which the guest already holds from a call that has
    /// succeeded whatever happens here (a clone's pidfd), as [`Self::add_fd`]
    /// does, except that a failure leaves the descriptor open and untracked
    /// rather than closing it: the guest keeps what Linux gave it.
    pub(crate) async fn track_held_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        flags: OFlag,
        ty: FdType,
    ) -> Result<(), Errno> {
        let host_stat = if guest.config().virtualize_metadata {
            Some(self.inject_fstat(guest, fd).await?)
        } else {
            None
        };
        guest
            .thread_state()
            .add_fd(fd, flags, ty, host_stat.map(Into::into))
    }

    /// [`Self::add_fd`], also returning the host `fstat` it took of `fd`, which
    /// it takes only when metadata is virtualized.
    async fn add_fd_with_stat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        flags: OFlag,
        ty: FdType,
    ) -> Result<Option<libc::stat>, Errno> {
        let host_stat = if guest.config().virtualize_metadata {
            match self.inject_fstat(guest, fd).await {
                Ok(stat) => Some(stat),
                Err(errno) => {
                    // `fd` is already open in the guest, but Detcore cannot
                    // model it: with metadata virtualization on, a descriptor
                    // without its stat is an invariant violation. The caller
                    // reports this error as the syscall's result, so close
                    // the descriptor rather than leave an untracked one open
                    // behind that error. Not retried on EINTR: Linux releases
                    // the descriptor even then, and a retry could close a
                    // reused number.
                    if let Err(close_errno) = guest.inject(syscalls::Close::new().with_fd(fd)).await
                    {
                        warn!(
                            "[detcore] could not close fd {} after failing to record its \
                             metadata ({}): {}",
                            fd, errno, close_errno
                        );
                    }
                    return Err(errno);
                }
            }
        } else {
            None
        };
        guest
            .thread_state()
            .add_fd(fd, flags, ty, host_stat.map(Into::into))?;
        Ok(host_stat)
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

    /// [`Self::observed_path`], with a `/proc` directory that `dirfd` names
    /// taken as the directory it is open on, as Linux looks a relative name up
    /// below that directory whoever holds the descriptor: a `/proc/self` or
    /// `/proc/thread-self` directory is the task the selector reached when it
    /// was opened (`DetFd::procfs_reached`), and a descriptor Detcore has no
    /// record of (one received over `SCM_RIGHTS`, which a fresh discovery
    /// leaves without one) is named as the kernel names the directory it is
    /// open on, where that is on procfs ([`Self::unrecorded_procfs_directory`]).
    /// Detcore's own decisions about procfs names (the files it renders, the
    /// `--proc` level, the links it canonicalizes) use this spelling. A name
    /// below the recorded directory of a task that is gone is ESRCH, and one
    /// below a selector link opened as the link itself ENOTDIR, as Linux
    /// answers them.
    pub(crate) async fn procfs_spelling_at<G: Guest<Self>>(
        &self,
        guest: &mut G,
        dirfd: i32,
        path: &Path,
    ) -> Result<PathBuf, Error> {
        if !path.is_absolute() && !path.as_os_str().is_empty() && dirfd != libc::AT_FDCWD {
            let (reached, link_itself) = guest
                .thread_state()
                .with_detfd(dirfd, |detfd| {
                    (detfd.procfs_reached(), detfd.procfs_link_itself())
                })
                .unwrap_or((None, false));
            // A selector link itself is no directory.
            if link_itself {
                return Err(Errno::ENOTDIR.into());
            }
            if let Some((reached, incarnation)) = reached {
                // The directory's task, while it lives: Linux checks the
                // task a pid directory was opened on at every lookup below it
                // (`proc_pid_permission`), and a task that is gone, whose
                // number another task may hold by now, fails every name with
                // ESRCH (measured on Linux 7.1: `cgroup`, `status`, `exe` and
                // a name no task has alike).
                // TODO-HUMAN-REVIEW(#4213): Review the stale task-directory
                // decision: a directory bound to a task identity at its open
                // answers ESRCH for every name once that identity no longer
                // holds its numbers (`ProcfsTaskAnswer::still_names`).
                if let Some(incarnation) = incarnation
                    && let Some(task) = procfs_reached_task(&reached)
                {
                    let answer = crate::tool_global::procfs_task(guest, task).await;
                    if !answer.still_names(incarnation) {
                        return Err(Errno::ESRCH.into());
                    }
                }
                return Ok(reached.join(path));
            }
            if let Some(directory) = self.unrecorded_procfs_directory(guest, dirfd).await {
                return Ok(directory.join(path));
            }
        }
        Self::observed_path(guest, dirfd, path)
    }

    /// The procfs directory descriptor `dirfd` is open on, where Detcore has
    /// no record of the descriptor, as the guest names it: the kernel's name
    /// for it (its `/proc/thread-self/fd/N` link, one injected `readlinkat`),
    /// if that is `/proc` or a path below it, with the kernel's numbers of
    /// the task it names taken back to the guest's where the two differ (DBT,
    /// `Self::translates_procfs_numbers`). Every backend's kernel beneath
    /// answers that link for the calling thread's own table: the host kernel
    /// on ptrace and DBT, KVM's provider (`guest_fd_path`), which has one
    /// table per process. `None` for a descriptor Detcore has a record of, any
    /// other object, a task number no task of the guest's holds, and where
    /// Detcore does not render procfs files (record and replay, after
    /// `chroot`).
    ///
    /// In every threading mode, `--no-sequentialize-threads` included. For a
    /// file Detcore renders, the link is the only observation of the
    /// descriptor, read once inside the guest's own call, and the answer comes
    /// from that name alone: another thread that replaces the descriptor
    /// concurrently is ordered before that read (the call looks below the new
    /// directory) or after it (below the old one), and Linux takes the
    /// descriptor at one point inside the call too (`path_init`), so each
    /// answer is one Linux can give. Without this a relaxed run's received
    /// `/proc` would leave its mount tables to the backend, which keeps no copy
    /// (KVM).
    ///
    /// Where the name only guides a call the kernel then resolves through the
    /// same descriptor, there are two observations, and without
    /// sequentialized threads another thread can replace the descriptor
    /// between them. A directory opened below it gets procfs provenance only
    /// if the descriptor the kernel returned holds a directory below `/proc`
    /// (`handle_openat`). The `--proc` level's refusal of an omitted name below
    /// it is best-effort there for `stat`, `access` and `readlink`, which have
    /// no check after the kernel's lookup: a residual of the relaxed mode,
    /// narrower than before, when no level applied below a received descriptor
    /// in that mode at all.
    pub(crate) async fn unrecorded_procfs_directory<G: Guest<Self>>(
        &self,
        guest: &mut G,
        dirfd: i32,
    ) -> Option<PathBuf> {
        if dirfd < 0 || !self.renders_procfs(guest) {
            return None;
        }
        let recorded = guest
            .thread_state()
            .with_detfd(dirfd, |detfd| detfd.path())
            .unwrap_or(None);
        if recorded.is_some() {
            return None;
        }
        const LINK_CAPACITY: usize = 40;
        const TARGET_CAPACITY: usize = 256;
        let mut stack = guest.stack().await;
        let link = stack.push(rendered_procfs::c_string::<LINK_CAPACITY>(
            format!("/proc/thread-self/fd/{dirfd}").as_bytes(),
        ));
        let target = stack.reserve::<[u8; TARGET_CAPACITY]>();
        let stack_guard = stack.commit().ok()?;
        let length = guest
            .inject(
                syscalls::Readlinkat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(link.as_raw() as *const libc::c_char))
                    .with_buf(Some(target.cast::<libc::c_char>()))
                    .with_buf_len(TARGET_CAPACITY),
            )
            .await;
        let bytes = match length.map(|length| length as usize) {
            Ok(length) if length < TARGET_CAPACITY => {
                let mut bytes = vec![0; length];
                guest
                    .memory()
                    .read_exact(target.cast::<u8>(), &mut bytes)
                    .ok()
                    .map(|()| bytes)
            }
            _ => None,
        };
        drop(stack_guard);
        let text = String::from_utf8(bytes?).ok()?;
        let (mut names, positions) = procfs_directory_task_positions(&text)?;
        if Self::translates_procfs_numbers(guest) && !positions.is_empty() {
            let kernel = positions
                .iter()
                .map(|position| names[*position].parse::<i32>().ok())
                .collect::<Option<Vec<_>>>()?;
            let translated = crate::tool_global::procfs_ids(guest, false, kernel).await;
            for (position, number) in positions.iter().zip(translated) {
                names[*position] = number?.to_string();
            }
        }
        let mut directory = PathBuf::from("/proc");
        directory.extend(names);
        Some(directory)
    }

    /// `path`, as a guest syscall named it relative to `dirfd`, made
    /// absolute in the guest's view. A relative spelling is not the object:
    /// absolute paths are already bound, a dirfd supplies its own prefix, and
    /// AT_FDCWD-relative spellings are resolved through the guest's root and
    /// cwd, so the result does not depend on Replayer's placeholder
    /// descriptor. A spelling that cannot be resolved is kept as given.
    fn observed_path<G: Guest<Self>>(guest: &G, dirfd: i32, path: &Path) -> Result<PathBuf, Error> {
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else if dirfd == libc::AT_FDCWD {
            resolved_at_fdcwd_path(guest.pid().as_raw(), path).unwrap_or_else(|| path.to_path_buf())
        } else {
            // A descriptor Detcore has no state for (one received over
            // `SCM_RIGHTS` under a backend whose tool does not discover
            // descriptors as they are used) keeps the relative spelling, which
            // the kernel looks up from it; the kernel, not this lookup,
            // decides whether the descriptor is open. Before, such an `openat`
            // failed with EBADF though the descriptor was open.
            guest
                .thread_state()
                .with_detfd(dirfd, |detfd| detfd.path())
                .unwrap_or(None)
                .map_or_else(|| path.to_path_buf(), |directory| directory.join(path))
        })
    }

    /// For `hermit run --verify`, report each path `call` may rebind in the
    /// guest's file namespace: both names of a rename, the new name of a link
    /// or symlink, an unlinked name, and a node, directory or file it makes
    /// or removes (see `detcore_model::host_input::HostMutationRecord`). A
    /// host file whose path the guest rebinds may have been replaced by the
    /// guest itself. Called before the call runs, whatever its outcome.
    pub(crate) async fn record_host_namespace_change<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) {
        let at = libc::AT_FDCWD;
        let changed: Vec<(i32, Option<syscalls::PathPtr>)> = match call {
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Rename(c) => vec![(at, c.oldpath()), (at, c.newpath())],
            Syscall::Renameat(c) => vec![(c.olddirfd(), c.oldpath()), (c.newdirfd(), c.newpath())],
            Syscall::Renameat2(c) => {
                vec![(c.olddirfd(), c.oldpath()), (c.newdirfd(), c.newpath())]
            }
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Link(c) => vec![(at, c.newpath())],
            Syscall::Linkat(c) => vec![(c.newdirfd(), c.newpath())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Unlink(c) => vec![(at, c.path())],
            Syscall::Unlinkat(c) => vec![(c.dirfd(), c.path())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Symlink(c) => vec![(at, c.linkpath())],
            Syscall::Symlinkat(c) => vec![(c.newdirfd(), c.linkpath())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Mknod(c) => vec![(at, c.path())],
            Syscall::Mknodat(c) => vec![(c.dirfd(), c.path())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Mkdir(c) => vec![(at, c.path())],
            Syscall::Mkdirat(c) => vec![(c.dirfd(), c.path())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Rmdir(c) => vec![(at, c.path())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Creat(c) => vec![(at, c.path())],
            _ => return,
        };
        for (dirfd, path) in changed {
            let observed = match path.map(|path| path.read(&guest.memory())) {
                Some(Ok(path)) => {
                    let path: PathBuf = path;
                    Self::observed_path(guest, dirfd, &path).ok()
                }
                _ => None,
            };
            crate::tool_global::record_host_mutation(guest, rebound_path(observed)).await;
        }
    }

    /// The host inodes whose last name `call` would remove: the name an
    /// `unlink`, `unlinkat` or `rmdir` removes, or the target a `rename`,
    /// `renameat` or `renameat2` replaces (not with `RENAME_EXCHANGE`), when
    /// it is a directory or a file with one link (see [`retires_on_removal`]).
    /// Each name is stat'ed with an injected `fstatat(AT_SYMLINK_NOFOLLOW)`
    /// before the call runs; a name that does not resolve removes nothing.
    /// Pass the result to [`Self::retire_removed_inodes`] if the call
    /// succeeds. Only with virtualized metadata, the only mode that numbers
    /// inodes.
    ///
    /// Nothing is retired without sequentialized threads. Only then do the
    /// lookups, the call and the retirement run in one turn; otherwise another
    /// thread can link, unlink or rename the file in between, and a file that
    /// keeps a name could be retired and renumbered (codex review of
    /// <https://github.com/rrnewton/hermit/pull/3849>, P2). Without them inode
    /// numbers behave as before retirement existed.
    pub(crate) async fn last_names_removed_by<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Vec<RawInode> {
        if !guest.config().virtualize_metadata || !guest.config().sequentialize_threads {
            return Vec::new();
        }
        let at = libc::AT_FDCWD;
        // The name removed, and for a rename the source, which replaces
        // nothing when it is the target's own file (`rename(a, a)`, or two
        // links of one file).
        let (removed, source) = match call {
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Unlink(c) => ((at, c.path()), None),
            Syscall::Unlinkat(c) => ((c.dirfd(), c.path()), None),
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Rmdir(c) => ((at, c.path()), None),
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Rename(c) => ((at, c.newpath()), Some((at, c.oldpath()))),
            Syscall::Renameat(c) => (
                (c.newdirfd(), c.newpath()),
                Some((c.olddirfd(), c.oldpath())),
            ),
            Syscall::Renameat2(c) if c.flags() & libc::RENAME_EXCHANGE == 0 => (
                (c.newdirfd(), c.newpath()),
                Some((c.olddirfd(), c.oldpath())),
            ),
            _ => return Vec::new(),
        };
        // Every path the call reads, which the lookups must leave unchanged.
        let inputs = [removed.1, source.and_then(|source| source.1)];
        let Some(path) = removed.1 else {
            return Vec::new();
        };
        let Ok(stat) = Self::inject_lstatat(guest, removed.0, path, inputs).await else {
            return Vec::new();
        };
        let inode = RawInode::new(stat.st_dev, stat.st_ino);
        if let Some((dirfd, Some(path))) = source
            && Self::inject_lstatat(guest, dirfd, path, inputs)
                .await
                .is_ok_and(|source| RawInode::new(source.st_dev, source.st_ino) == inode)
        {
            return Vec::new();
        }
        if retires_on_removal(&stat) {
            vec![inode]
        } else {
            Vec::new()
        }
    }

    /// The guest-made FIFO (`crate::fifo_peer::is_guest_made`) whose last
    /// name `call` removes: the name an `unlink` or `unlinkat` removes, or the
    /// target a `rename`, `renameat` or `renameat2` replaces (not with
    /// `RENAME_EXCHANGE`), when it is that FIFO's only link
    /// ([`retires_on_removal`]) and not the rename's own source (another link
    /// of the same FIFO, or `rename(a, a)`). Stat'ed before the call runs; pass
    /// the result to `crate::fifo_peer::forget_guest_made` if the call
    /// succeeds, so that a FIFO that later reuses the number is not taken for
    /// a guest's, while a FIFO that keeps a name stays a guest's. Only where
    /// the set is shared by every guest (`fifo_peer::guest_descriptors_are_visible`).
    pub(crate) async fn guest_made_fifos_removed_by<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Vec<(u64, u64)> {
        if !crate::fifo_peer::any_guest_made()
            || !crate::fifo_peer::guest_descriptors_are_visible(
                &self.cfg.backend,
                guest.thread_state().physical_tid,
            )
        {
            return Vec::new();
        }
        let at = libc::AT_FDCWD;
        let (removed, source) = match call {
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Unlink(c) => ((at, c.path()), None),
            Syscall::Unlinkat(c) => ((c.dirfd(), c.path()), None),
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Rename(c) => ((at, c.newpath()), Some((at, c.oldpath()))),
            Syscall::Renameat(c) => (
                (c.newdirfd(), c.newpath()),
                Some((c.olddirfd(), c.oldpath())),
            ),
            Syscall::Renameat2(c) if c.flags() & libc::RENAME_EXCHANGE == 0 => (
                (c.newdirfd(), c.newpath()),
                Some((c.olddirfd(), c.oldpath())),
            ),
            _ => return Vec::new(),
        };
        let inputs = [removed.1, source.and_then(|source| source.1)];
        let Some(path) = removed.1 else {
            return Vec::new();
        };
        let Ok(stat) = Self::inject_lstatat(guest, removed.0, path, inputs).await else {
            return Vec::new();
        };
        let fifo = (stat.st_dev, stat.st_ino);
        if stat.st_mode & libc::S_IFMT != libc::S_IFIFO
            || !crate::fifo_peer::is_guest_made(fifo)
            || !retires_on_removal(&stat)
        {
            return Vec::new();
        }
        if let Some((dirfd, Some(path))) = source
            && Self::inject_lstatat(guest, dirfd, path, inputs)
                .await
                .is_ok_and(|source| (source.st_dev, source.st_ino) == fifo)
        {
            return Vec::new();
        }
        vec![fifo]
    }

    /// After a successful `mknod` or `mknodat` that made a FIFO, record it as
    /// a guest's (`crate::fifo_peer::note_guest_made`), stat'ed at the name the
    /// call made, in the same turn. Only where a blocking open of it can wait
    /// in polling turns (sequentialized `hermit run` under a separate tracer
    /// whose guest thread IDs are host tasks).
    pub(crate) async fn note_guest_made_fifo<G: Guest<Self>>(&self, guest: &mut G, call: &Syscall) {
        // Only where one set serves every guest, as `fifo_at` requires.
        if !self.cfg.use_nonblocking_sockets()
            || self.cfg.recordreplay_modes
            || !crate::fifo_peer::guest_descriptors_are_visible(
                &self.cfg.backend,
                guest.thread_state().physical_tid,
            )
        {
            return;
        }
        let (dirfd, path, mode) = match call {
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Mknod(c) => (libc::AT_FDCWD, c.path(), c.mode().bits()),
            Syscall::Mknodat(c) => (c.dirfd(), c.path(), c.mode().bits()),
            _ => return,
        };
        if mode & libc::S_IFMT != libc::S_IFIFO {
            return;
        }
        let Some(path) = path else {
            return;
        };
        if let Ok(stat) = Self::inject_lstatat(guest, dirfd, path, [Some(path), None]).await
            && stat.st_mode & libc::S_IFMT == libc::S_IFIFO
        {
            crate::fifo_peer::note_guest_made((stat.st_dev, stat.st_ino));
        }
    }

    /// Retire the mappings of `inodes`, whose last names a successful call
    /// removed (see [`Self::last_names_removed_by`] and `InodePool::retire`).
    pub(crate) async fn retire_removed_inodes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        inodes: Vec<RawInode>,
    ) {
        for inode in inodes {
            retire_inode(guest, inode).await;
        }
    }

    /// For `hermit run --verify` on a backend whose guest holds code that
    /// makes untraced syscalls (`Config::untraced_code_range`): report `/`
    /// when `call` may change the memory holding it (see
    /// [`may_change_untraced_code`]). Intact, that code faults right after
    /// its syscall, and the fault is reported (`handle_signal_event`); a
    /// guest that rewrote or replaced it could use it without faulting, so
    /// Detcore could not establish what the guest changed, and `/`, the
    /// ancestor of every path, keeps a host input change from being named
    /// in that run. Called before the call runs, whatever its outcome.
    pub(crate) async fn record_untraced_code_change<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) {
        let Some(range) = guest.config().untraced_code_range else {
            return;
        };
        let (number, args) = call.into_parts();
        if may_change_untraced_code(number, &args, range, default_huge_page_size()) {
            crate::tool_global::record_host_mutation(guest, String::from("/")).await;
        }
    }

    /// Open an `exe` link, by `call`, as the image Detcore recorded for the
    /// process it reaches, `exe` (`Detcore::launched_exe`): Linux opens the
    /// image the link names, which the kernel's link here does not (it names
    /// the launcher). The kernel first opens the guest's spelling itself as
    /// a path (`O_PATH`), so whether the link is there and every error stay
    /// its own (an exited thread-group leader's `ENOENT`). Without a record,
    /// or for an image no longer at its path, which only the kernel's link to
    /// the image could still open, the run is refused by name.
    async fn open_launched_exe<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
        exe: Option<&crate::exec_record::ExecRecord>,
    ) -> Result<i64, Error> {
        // The longest path Linux opens, NUL included.
        const CAPACITY: usize = libc::PATH_MAX as usize;
        let probe = syscalls::Openat::new()
            .with_dirfd(call.dirfd())
            .with_path(call.path())
            .with_flags(OFlag::O_PATH | OFlag::O_CLOEXEC);
        let probed = guest.inject(probe).await?;
        let _ = guest
            .inject(syscalls::Close::new().with_fd(probed as RawFd))
            .await;
        let present = exe.filter(|exe| {
            self.guest_root(guest)
                .is_some_and(|root| exe.is_present(&root))
        });
        let Some(exe) = present else {
            return self.refuse_launched_exe(guest, Sysno::openat, exe).await;
        };
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(exe.exe.as_os_str());
        let mut stack = guest.stack().await;
        if bytes.len() >= CAPACITY || stack.capacity() < CAPACITY {
            drop(stack);
            let reason = format!(
                "unsupported: opening the /proc/<pid>/exe link of {:?}, whose path does not \
                 fit the guest stack, where the kernel's link names the launcher",
                exe.exe
            );
            return self
                .refuse_launched_exe_because(guest, Sysno::openat, reason)
                .await;
        }
        let mut buffer = [0_u8; CAPACITY];
        buffer[..bytes.len()].copy_from_slice(bytes);
        let address = stack.push(buffer).cast::<libc::c_char>();
        let stack_guard = stack.commit()?;
        let call = call.with_path(PathPtr::from_ptr(address.as_raw() as *const libc::c_char));
        let result = self.record_or_replay(guest, Syscall::Openat(call)).await;
        drop(stack_guard);
        Ok(result?)
    }

    /// Whether guest descriptor `fd` is open on the launcher a launched run
    /// declared (`Detcore::launched_exe`), read where the tool shares the
    /// guest's descriptor table, as a launcher's tool does. Asked only for an
    /// open of a possible `exe` link
    /// (`crate::syscalls::namespace::exe_link_candidate`).
    fn opened_the_launcher(&self, fd: RawFd) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Some(launcher) = self.declared_launcher() else {
            return false;
        };
        if !self.cfg.backend.tool_shares_guest_descriptor_table {
            return false;
        }
        let mut opened: libc::stat = unsafe { std::mem::zeroed() };
        // Runtime-owned read-only inspection of the shared descriptor table.
        let result = unsafe {
            crate::util::raw_syscall(
                libc::SYS_fstat,
                [fd as u64, (&raw mut opened) as u64, 0, 0, 0, 0],
            )
        };
        result == 0
            && std::fs::metadata(launcher)
                .is_ok_and(|meta| (meta.dev(), meta.ino()) == (opened.st_dev, opened.st_ino))
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
        let observed_path = Self::observed_path(guest, call.dirfd(), &path)?;
        // The spelling Detcore's own lookups of the files it renders see.
        let procfs_spelling = self.procfs_spelling_at(guest, call.dirfd(), &path).await?;
        crate::race_probe::delay(crate::race_probe::PROCFS_SPELLING_RESOLVED).await;
        if guest.config().record_host_inputs && call.flags().contains(OFlag::O_CREAT) {
            // It may create the file it opens. Reported before the open runs,
            // as `record_host_namespace_change` reports the other changes.
            crate::tool_global::record_host_mutation(
                guest,
                rebound_path(Some(observed_path.clone())),
            )
            .await;
        }

        let resource = ResourceID::Path(path.clone());
        // Ask for permission to resolve this path into a file:
        let request = guest.thread_state().mk_request(resource, Permission::R);
        resource_request(guest, request).await;
        // An empty path names nothing to `openat`, which has no
        // `AT_EMPTY_PATH`: the kernel's ENOENT.
        let rendered = match if path.as_os_str().is_empty() {
            Ok(None)
        } else {
            self.rendered_procfs_at(guest, &procfs_spelling).await
        } {
            Ok(rendered) => rendered,
            Err(error) => {
                resource_release_all(guest).await;
                // Flags `build_open_flags` refuses fail before the lookup.
                return Err(
                    rendered_procfs::open_flags_refused(call.flags()).map_or(error, Into::into)
                );
            }
        };
        // `O_PATH|O_NOFOLLOW` opens a descriptor link itself, which the
        // kernel has.
        if let Some(target) = rendered.filter(|target| {
            !(target.via_link && call.flags().contains(OFlag::O_PATH | OFlag::O_NOFOLLOW))
        }) {
            let result = self.open_rendered_procfs(guest, call.flags(), target).await;
            resource_release_all(guest).await;
            return result;
        }
        // An `exe` link Detcore answers from its record (`Detcore::launched_exe`)
        // opens that image, for reading only: a write open of a running image
        // is the kernel's `ETXTBSY`, `O_NOFOLLOW` its `ELOOP`, and `O_PATH`
        // with it the link itself.
        let launched_exe = if call.flags().bits() & libc::O_ACCMODE == libc::O_RDONLY
            && !call
                .flags()
                .intersects(OFlag::O_NOFOLLOW | OFlag::O_TRUNC | OFlag::O_CREAT)
        {
            self.launched_exe(guest, (call.dirfd(), &path), true).await
        } else {
            None
        };
        // Signal phase 1 (`sigalrm_phase1`): in a process that handles SIGALRM,
        // an open that may wait (a FIFO, a character or block device) is
        // refused. Checked after the grant: no other guest runs before the open.
        if guest.thread_state().sigalrm_handled
            && !call.flags().intersects(OFlag::O_DIRECTORY | OFlag::O_PATH)
            && let Some(path) = call.path()
            && crate::sigalrm_phase1::open_refused(
                Self::inject_statat(
                    guest,
                    call.dirfd(),
                    path,
                    [Some(path), None],
                    AtFlags::empty(),
                )
                .await,
                call.flags().contains(OFlag::O_NONBLOCK),
            )
        {
            return Err(Errno::EOPNOTSUPP.into());
        }
        // The descriptor a descriptor link in the spelling names, as the open
        // finds it now, in its own turn, where the kernel looks the link up
        // (`LinkSource`). The open below may block and leave the turn.
        let link_source = crate::syscalls::namespace::own_descriptor_link(
            &observed_path,
            self.procfs_caller_ids(guest),
        )
        .and_then(|link| {
            guest
                .thread_state()
                .with_detfd(link.fd, |detfd| LinkSource {
                    fd: link.fd,
                    path: detfd.path(),
                    open_file: detfd.open_file_id(),
                    reached: detfd.procfs_reached(),
                })
                .ok()
        });
        // A blocking open, for reading or for writing, of a FIFO a guest made
        // waits in polling turns for a guest at the other end
        // (https://github.com/rrnewton/hermit/issues/2203).
        let fifo = (self.cfg.use_nonblocking_sockets()
            && !self.cfg.recordreplay_modes
            && !call.flags().intersects(OFlag::O_NONBLOCK | OFlag::O_PATH)
            && call.flags().bits() & libc::O_ACCMODE != libc::O_RDWR)
            .then(|| {
                crate::fifo_peer::fifo_at(
                    &self.cfg.backend,
                    guest.thread_state().physical_tid,
                    guest.tid().as_raw(),
                    call.dirfd(),
                    &path,
                )
            })
            .flatten()
            // Only a FIFO a guest made; every other one is opened as before.
            .filter(|(fifo, _)| crate::fifo_peer::is_guest_made(*fifo));
        // A later descriptor lookup proves the object, while this observation
        // brackets its kernel open against target exec/PID replacement.
        let maps_before = if guest.runtime_private_memory_regions().is_some() {
            if let Some((pid, tid)) = self.procfs_caller_ids(guest)
                && let Some(task) = crate::procfs::maps_task_at(&procfs_spelling, pid, tid)
            {
                Some((task, crate::tool_global::procfs_task(guest, task).await))
            } else {
                None
            }
        } else {
            None
        };
        let res: Result<i64, Error> = match fifo {
            Some((fifo, fifo_path)) => {
                self.open_fifo_once_a_guest_holds_its_peer(guest, call, fifo, &fifo_path)
                    .await
            }
            None => match launched_exe {
                Some(exe) => self.open_launched_exe(guest, call, exe.as_deref()).await,
                None => self
                    .record_or_replay(guest, Syscall::Openat(call))
                    .await
                    .map_err(Error::from),
            },
        };

        match res {
            Ok(fd) => {
                let fd = fd as RawFd;
                // A link Detcore could not resolve whose kernel open reached
                // the launcher, in the guest's image's place.
                if crate::syscalls::namespace::exe_link_candidate(&path)
                    && self.opened_the_launcher(fd)
                {
                    let _ = guest.inject(syscalls::Close::new().with_fd(fd)).await;
                    resource_release_all(guest).await;
                    return self.refuse_launched_exe(guest, Sysno::openat, None).await;
                }
                // Another process's descriptor link, or another spelling of
                // this one's, to a placeholder: the guest opened the rendered
                // file again, as Linux opens the procfs file again.
                // (Where one tool process serves every guest, it knows every
                // placeholder, and a run that made none pays nothing.)
                if names_descriptor_link(&observed_path)
                    && !call.flags().contains(OFlag::O_PATH | OFlag::O_NOFOLLOW)
                    && (rendered_procfs::placeholders_exist()
                        || guest.config().backend.tool_shares_guest_descriptor_table)
                {
                    let behind = self.rendered_procfs_behind(guest, fd).await;
                    if !matches!(behind, Ok(None)) {
                        let _ = guest.inject(syscalls::Close::new().with_fd(fd)).await;
                    }
                    let result = match behind {
                        Ok(None) => None,
                        Ok(Some(path)) => {
                            let target = rendered_procfs::RenderedTarget {
                                procfs: ProcfsFile::from_rendered_path(path)
                                    .expect("a rendered path is a procfs file"),
                                via_link: true,
                            };
                            Some(self.open_rendered_procfs(guest, call.flags(), target).await)
                        }
                        Err(error) => Some(Err(error)),
                    };
                    if let Some(result) = result {
                        resource_release_all(guest).await;
                        return result;
                    }
                }
                // Under a network trace mode only; the run ends before the
                // guest sees the descriptor.
                let pid = guest.pid().as_raw();
                let backend = self.cfg.backend;
                self.network_check_open(guest, &observed_path, || {
                    resolved_open_path(&backend, pid, fd)
                })
                .await;
                let fd_type = path.to_str().map_or(FdType::Regular, |fname| {
                    if fname == "/dev/random" || fname == "/dev/urandom" {
                        FdType::Rng
                    } else {
                        FdType::Regular
                    }
                });
                // A guest-created pipe reopened by path (`/dev/stdin`,
                // `/proc/self/fd/N`, bash's `< <(cmd)` as `/dev/fd/63`) is a NEW open
                // file description, so it carries neither the Pipe type nor the
                // physical O_NONBLOCK that `handle_pipe2` gave the original. Left as a
                // physically blocking Regular fd, a read that waits for a writer blocks
                // in the kernel while holding the scheduler turn, and the writer never
                // runs (https://github.com/rrnewton/hermit/issues/1850). Give it the
                // same treatment as `handle_pipe2`: Pipe type plus a Detcore-internal
                // physical O_NONBLOCK, which F_GETFL hides from the guest. Only a pipe
                // this process already holds as scheduler-managed qualifies; a host
                // pipe or a named FIFO keeps `deterministic_read`'s Regular handling.
                // Gated like the forced O_NONBLOCK in F_SETFL: Replayer's descriptor
                // is an eventfd placeholder, so record and replay would not classify
                // alike.
                let fd_type = if fd_type == FdType::Regular
                    && self.cfg.use_nonblocking_sockets()
                    && !self.cfg.recordreplay_modes
                    && !call.flags().contains(OFlag::O_PATH)
                    && reopens_scheduler_managed_pipe(
                        &self.cfg.backend,
                        guest.pid().as_raw(),
                        fd,
                        &guest.thread_state().scheduler_managed_pipe_fds(),
                    )
                    && (call.flags().contains(OFlag::O_NONBLOCK)
                        || self.inject_physical_nonblocking(guest, fd).await.is_ok())
                {
                    FdType::Pipe
                } else {
                    fd_type
                };
                let host_stat = self
                    .add_fd_with_stat(guest, fd, call.flags(), fd_type)
                    .await?;
                // A placeholder reached through any other spelling (a symbolic
                // link to a descriptor link, `/dev/stdin`): the kernel opened
                // the placeholder, which is the rendered file again, also for
                // `O_PATH`. The kernel already followed every link, so none
                // remains for `O_NOFOLLOW` to refuse (an `O_PATH|O_NOFOLLOW`
                // descriptor for a link is the link, no placeholder).
                // Without virtualized metadata no stat was taken; take one
                // while a placeholder may be about.
                let identity = match &host_stat {
                    Some(stat) => Some(*stat),
                    None if self.renders_procfs(guest)
                        && (guest.thread_state().holds_a_rendered_procfs_file()
                            || rendered_procfs::placeholders_exist()) =>
                    {
                        self.inject_fstat(guest, fd).await.ok()
                    }
                    None => None,
                };
                let reached = match &identity {
                    Some(stat) if self.renders_procfs(guest) => {
                        self.recognize_placeholder(
                            guest,
                            &DetStat::from(*stat),
                            StatTarget::Descriptor(fd),
                        )
                        .await
                    }
                    _ => None,
                };
                if let Some(path) = reached {
                    guest.thread_state().remove_fd(fd);
                    let _ = guest.inject(syscalls::Close::new().with_fd(fd)).await;
                    let target = rendered_procfs::RenderedTarget {
                        procfs: ProcfsFile::from_rendered_path(path)
                            .expect("a rendered path is a procfs file"),
                        via_link: false,
                    };
                    let result = self.open_rendered_procfs(guest, call.flags(), target).await;
                    resource_release_all(guest).await;
                    return result;
                }
                // A `/proc` name outside the run's level that the kernel
                // reached through a spelling the lookup check does not
                // resolve (a symbolic link, `..`), named by the kernel's path
                // for the descriptor (see `refuse_procfs_name_outside_level`).
                if self
                    .opened_procfs_name_outside_level(
                        guest,
                        fd,
                        identity.as_ref().map(|stat| (stat.st_dev, stat.st_ino)),
                    )
                    .await
                {
                    guest.thread_state().remove_fd(fd);
                    let _ = guest.inject(syscalls::Close::new().with_fd(fd)).await;
                    resource_release_all(guest).await;
                    return Err(Errno::ENOENT.into());
                }
                // A FIFO (a named one, or a pipe reached by path that the
                // check above left `Regular`) stays `Regular` and physically
                // blocking, so a FIFO whose other end is on the host blocks as
                // before. Its first blocking read or write that finds a guest
                // holding the other end makes it an internally polled pipe
                // (`fifo_polls_if_a_guest_holds_its_peer`). Run mode only, as
                // for a reopened pipe: Replayer's descriptor is a placeholder.
                // `fifo_of` finds a FIFO only where it can read the guest's
                // descriptors through `/proc/<tid>`: under a separate tracer
                // (not DBT, in its PID container) whose guest thread IDs are
                // host tasks (not KVM). Elsewhere a FIFO blocks as before.
                if fd_type == FdType::Regular
                    && self.cfg.use_nonblocking_sockets()
                    && !self.cfg.recordreplay_modes
                    && !call.flags().contains(OFlag::O_PATH)
                    && let Some(fifo) = crate::fifo_peer::fifo_of(
                        &self.cfg.backend,
                        guest.thread_state().physical_tid,
                        guest.tid().as_raw(),
                        fd,
                    )
                {
                    guest
                        .thread_state()
                        .with_detfd(fd, |detfd| detfd.mark_fifo_awaiting_guest_peer())?;
                    crate::fifo_peer::note_guest_open(fifo, call.flags().bits());
                    // Name the FIFO for `--pipe-wake-latency`, and ready the
                    // opens waiting for the end this one opened.
                    if self.cfg.pipe_wake_latency_in_effect().is_some() {
                        let accmode = call.flags().bits() & libc::O_ACCMODE;
                        let end = crate::pipe_wake::ChannelEnd {
                            channel: crate::pipe_wake::ChannelId::Fifo(
                                crate::fifo_peer::fifo_channel(fifo),
                            ),
                            reads: accmode != libc::O_WRONLY,
                            writes: accmode != libc::O_RDONLY,
                        };
                        guest
                            .thread_state()
                            .with_detfd(fd, |detfd| detfd.set_channel(end))?;
                        for (applies, readies) in [
                            (end.reads, crate::pipe_wake::Wants::PeerReader),
                            (end.writes, crate::pipe_wake::Wants::PeerWriter),
                        ] {
                            if applies {
                                self.note_pipe_readied(guest, end.channel, readies).await;
                            }
                        }
                    }
                }
                if let (true, Some(stat)) = (guest.config().record_host_inputs, &host_stat) {
                    crate::tool_global::record_host_input(
                        guest,
                        observed_path.to_string_lossy().into_owned(),
                        detcore_model::host_input::HostFileIdentity::from_stat(stat),
                    )
                    .await;
                }
                // An open that finds its file linked, or creates it
                // (`O_TMPFILE`), reaches a file that has a name or never had
                // one, so a retired mapping of its inode described a file the
                // host has freed (https://github.com/rrnewton/hermit/issues/3840).
                // Reopening an unlinked file through `/proc/self/fd/N` finds no
                // link and keeps it.
                if let Some(stat) = &host_stat
                    && (stat.st_nlink > 0 || call.flags().contains(OFlag::O_TMPFILE))
                {
                    // A procfs entry the host rebuilt keeps its number when it is
                    // next written or read through this descriptor.
                    let procfs =
                        Self::procfs_entry(guest, &DetStat::from(stat), StatTarget::Descriptor(fd))
                            .await;
                    forget_retired_inode(
                        guest,
                        RawInode::new(stat.st_dev, stat.st_ino),
                        procfs.name(stat.st_dev),
                    )
                    .await;
                }
                // A descriptor that writes process memory can rewrite the
                // untraced code (see `record_untraced_code_change`). Reported
                // once the open returns, before the guest has the descriptor;
                // the record has no position in the run, so it covers every
                // write through it.
                if guest.config().record_host_inputs
                    && guest.config().untraced_code_range.is_some()
                    && may_write_process_memory(
                        call.flags(),
                        descriptor_identity(guest.tid().as_raw(), fd)
                            .as_ref()
                            .map(|(link, on_procfs)| (link.as_path(), *on_procfs)),
                    )
                {
                    crate::tool_global::record_host_mutation(guest, String::from("/")).await;
                }
                // A session leader with no controlling terminal that opens a
                // terminal without O_NOCTTY gains it as one, and the terminal
                // can then signal the session and its foreground process group
                // at moments set by host timing (`terminal_signals`). Hold
                // those signals in every process, as `handle_ioctl` does for
                // TIOCSCTTY and TIOCGPTPEER. Only the kernel knows whether this
                // open made the terminal controlling, so the record is made
                // after the call; it is still this thread's turn, and no other
                // thread's gated wait reads the record before the turn ends.
                // Skipped under record and replay: Replayer's descriptor is a
                // placeholder that never becomes a terminal, so the two runs
                // would not record alike.
                if guest.config().backend.host_timed_signals_are_physical
                    && !self.cfg.recordreplay_modes
                    && open_can_acquire_controlling_terminal(call.flags())
                    && opened_controlling_terminal(guest.pid().as_raw(), fd, host_stat.as_ref())
                {
                    record_host_timed_signals(
                        guest,
                        HostTimedSignalScope::Container,
                        terminal_signals(),
                    )
                    .await;
                }
                if fd_type == FdType::Pipe {
                    self.maybe_set_nonblocking_fd(guest, fd);
                }
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
                //
                // Under record and replay the descriptor's link is read through
                // the recording: replay holds the descriptor's number with an
                // eventfd, which resolves to no procfs file, so both runs must
                // classify the open by the link the recording read
                // (`recorded_open_path`).
                // A process's `auxv` the kernel opened keeps the kernel's
                // bytes, unclassified, as before Detcore rendered the file
                // (`ProcfsFile::kernel_served`): by its spelling here, and by
                // its link below.
                let mut procfs =
                    ProcfsFile::from_path(&observed_path).filter(ProcfsFile::kernel_served);
                // Under record and replay the link is read for every open,
                // also one whose spelling classifies: a spelling through `..`
                // (`/proc/self/../self/io`) classifies as the file it reaches,
                // which may be outside the level.
                if procfs.is_none() || self.cfg.recordreplay_modes {
                    let resolved = if self.cfg.recordreplay_modes {
                        self.recorded_open_path(guest, fd).await?
                    } else if crate::procfs_inode::tool_can_name_guest_descriptors(
                        &self.cfg.backend,
                    ) {
                        resolved_open_path(&self.cfg.backend, guest.pid().as_raw(), fd)
                    } else if call.dirfd() != libc::AT_FDCWD && !path.is_absolute() {
                        // Where the tool cannot read the guest's descriptor
                        // links itself, it asks the kernel beneath the backend
                        // for the same link (`descriptor_kernel_link`, one
                        // injected `readlinkat`), but only for a lookup
                        // relative to a directory descriptor: its prefix is
                        // that directory's recorded spelling, which need not
                        // name the directory (a symbolic link, a `..`, a `.`
                        // after `chdir`, a descriptor received over a
                        // socket), so `openat(dirfd, "cpu0/cpufreq/boost")`
                        // must classify by the object as it does where the
                        // tool reads the link. Absolute and AT_FDCWD-relative
                        // spellings stay lexical there, so that an
                        // unclassified open costs no injected call. Without
                        // sequentialized threads, a sibling's dup2 over the
                        // descriptor before the install below can misattach
                        // the class, as for a spelling-classified open on
                        // every backend
                        // (https://github.com/rrnewton/hermit/issues/4543).
                        Self::descriptor_kernel_link(guest, fd)
                            .await
                            .filter(|link| link.is_absolute())
                    } else {
                        None
                    };
                    // Under record and replay, a `/proc` name outside the
                    // run's level that a spelling the lookup check does not
                    // resolve reached, named by the link the recording read
                    // (in the guest's view, also after a `chroot`), so both
                    // runs refuse it alike (outside record and replay the
                    // check after the open above does it). Only where the
                    // recording saw that the descriptor is procfs: after a
                    // `chroot`, or under a user bind, `/proc` need not be.
                    if self.cfg.recordreplay_modes
                        && resolved.as_deref().is_some_and(|resolved| {
                            levels::procfs_name(resolved)
                                .is_some_and(|name| name.absent_at(self.cfg.proc_level))
                        })
                        // And the descriptor is procfs, not a user bind over
                        // part of `/proc`, as the recording saw it.
                        && self.recorded_descriptor_is_on_procfs(guest, fd).await
                    {
                        guest.thread_state().remove_fd(fd);
                        let _ = self
                            .record_or_replay(guest, syscalls::Close::new().with_fd(fd))
                            .await;
                        resource_release_all(guest).await;
                        return Err(Errno::ENOENT.into());
                    }
                    if procfs.is_none() {
                        procfs = resolved
                            .filter(|resolved| resolved != &observed_path)
                            .and_then(|resolved| ProcfsFile::from_path(&resolved))
                            .filter(ProcfsFile::kernel_served);
                    }
                }
                // A process's cgroup file bind-mounted at another path, or
                // reached through another procfs mount, classifies by neither
                // its spelling nor its link, which names the bind's mount
                // point, and would read as the host's bytes. Name the procfs
                // entry the descriptor reaches, as a stat does, and classify
                // that name, so the alias is the file it is (Codex review of
                // https://github.com/rrnewton/hermit/pull/4386). Only that
                // kind (`ProcfsFile::may_be_named_by_provenance`): a mountinfo
                // file of another procfs mount failed its identity check and
                // stopped the run (Claude re-check). Like the link above, this
                // only adds a classification. Record and replay name no
                // entries, and without sequentialized threads naming records a
                // determinism loss for any file that may be procfs, so neither
                // looks.
                if procfs.is_none()
                    && guest.config().sequentialize_threads
                    && let Some(stat) = host_stat
                    && let ProcfsEntry::At(within) =
                        Self::procfs_entry(guest, &DetStat::from(stat), StatTarget::Descriptor(fd))
                            .await
                {
                    procfs = ProcfsFile::from_path(
                        &Path::new("/proc").join(within.strip_prefix("/").unwrap_or(&within)),
                    )
                    .filter(ProcfsFile::may_be_named_by_provenance);
                }
                if guest.runtime_private_memory_regions().is_some()
                    && procfs.as_ref().is_some_and(ProcfsFile::is_maps)
                    && let Some(stat) = host_stat
                {
                    match self
                        .maps_descriptor_entry(guest, &DetStat::from(stat), fd)
                        .await
                    {
                        ProcfsEntry::At(within) => {
                            let actual = Path::new("/proc")
                                .join(within.strip_prefix("/").unwrap_or(&within));
                            if let Some(task) = self.maps_guest_target(guest, &actual).await
                                && let Some((before_task, before)) = maps_before.as_ref()
                                && *before_task == task
                            {
                                let after = crate::tool_global::procfs_task(guest, task).await;
                                if let Some(target) =
                                    crate::procfs::MapsTarget::from_open(task, before, &after)
                                    && guest.runtime_private_memory_is_current(&target.private)
                                {
                                    procfs.as_mut().unwrap().bind_maps_target(target);
                                }
                            }
                        }
                        // A shadowed proc spelling is an ordinary file. Its
                        // contents must not receive procfs projection at all.
                        ProcfsEntry::NotProcfs => procfs = None,
                        ProcfsEntry::Unnamed | ProcfsEntry::Unknown => {}
                    }
                }
                // A process's `mountinfo` or `mounts` the kernel opened reads
                // the view its task's root gives it, decided now, at the open,
                // as Linux takes the root at the open.
                if let Some(file) = procfs.as_ref()
                    && file.mount_table_rendering().is_some()
                {
                    let kernel = self.mount_view_is_kernel(guest, file).await;
                    procfs
                        .as_mut()
                        .expect("a classified procfs file disappeared")
                        .pin_mount_view(kernel);
                }
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
                // Signal phase 1: what the kernel says this file is, for the
                // table's descriptor rules (`sigalrm_phase1`).
                let provenance = guest
                    .thread_state()
                    .sigalrm_handled
                    .then(|| crate::sigalrm_phase1::kernel_provenance(guest.pid().as_raw(), fd));
                // An open through one of the caller's descriptor links
                // records the path of the descriptor it reopens, so that a
                // name relative to it (`openat(fd, "stat")` with `fd` from
                // `open("/proc/self/fd/<a /proc descriptor>")`) is the file
                // Linux finds there (see `path_to_record_for_open`, which
                // also keeps the spelling wherever the link may reach
                // another file). Not after a `chroot`, where `/proc` and
                // `/dev/fd` need not be what they are outside it.
                //
                // The leader holds the table procfs reads for
                // `/proc/self/fd`, and so does every thread that holds the
                // same table: the process's, which the group shares
                // (`process_files`). Not known only for a state restored
                // from before that was kept, where only the leader is shown
                // to hold it.
                let opener = {
                    let detpid = self.own_detpid(guest);
                    let state = guest.thread_state();
                    DescriptorLinkOpener {
                        own: self.procfs_caller_ids(guest),
                        holds_process_table: state.dettid == detpid || state.holds_process_files(),
                    }
                };
                // Where `N`, in the caller's own table, has no recorded path
                // at all (a descriptor received through SCM_RIGHTS, which
                // Detcore does not track, or one discovered without one), a
                // procfs directory is named as the kernel beneath the backend
                // names the opened descriptor (`procfs_directory_named_for`,
                // one injected `readlinkat`). Only where Detcore renders
                // procfs files (not under record and replay, which does not
                // record that call, nor after a `chroot`), and with
                // sequentialized threads, so that no other guest thread can
                // replace the descriptor before the link is read.
                let unrecorded_link_identity = if self.renders_procfs(guest)
                    && guest.config().sequentialize_threads
                    && !opens_the_link_itself(&observed_path, call.flags())
                    && host_stat.is_none_or(|stat| stat.st_mode & libc::S_IFMT == libc::S_IFDIR)
                    && link_source
                        .as_ref()
                        .is_none_or(|source| source.path.is_none())
                    && crate::syscalls::namespace::own_descriptor_link(&observed_path, opener.own)
                        .is_some_and(|link| {
                            link.table == crate::syscalls::namespace::DescriptorTable::Thread
                                || opener.holds_process_table
                        }) {
                    Self::procfs_directory_named_for(guest, fd).await
                } else {
                    None
                };
                let recorded_path = {
                    let state = guest.thread_state();
                    if state.changed_root.load(Ordering::SeqCst) {
                        observed_path.clone()
                    } else {
                        // `N` as the lookup saw it, and only if `N` still holds
                        // the same open file description: a thread sharing
                        // the table may have replaced it while the open
                        // waited (`LinkSource`).
                        path_to_record_for_open(
                            &observed_path,
                            call.flags(),
                            opener,
                            |target| {
                                link_source.as_ref().and_then(|source| {
                                    source.path_if_unchanged(
                                        target,
                                        state.with_detfd(target, |detfd| detfd.open_file_id()).ok(),
                                    )
                                })
                            },
                            unrecorded_link_identity,
                        )
                    }
                };
                // A `/proc/self` or `/proc/thread-self` directory, opened by
                // that spelling or through a descriptor link to one, is the
                // task the selector reached at its first open; a name below it
                // is that task's file whoever looks it up (a fork's child, a
                // thread). Recorded only where Detcore renders per-process
                // files, which read it.
                let link_itself = self.renders_procfs(guest)
                    && opens_the_link_itself(&procfs_spelling, call.flags())
                    && names_a_selector_link(&procfs_spelling);
                let reached = if self.renders_procfs(guest) {
                    let linked = {
                        let state = guest.thread_state();
                        crate::syscalls::namespace::own_descriptor_link(&observed_path, opener.own)
                            .filter(|link| {
                                !opens_the_link_itself(&observed_path, call.flags())
                                    && (link.table
                                        == crate::syscalls::namespace::DescriptorTable::Thread
                                        || opener.holds_process_table)
                            })
                            .and_then(|link| {
                                link_source.as_ref().and_then(|source| {
                                    source.reached_if_unchanged(
                                        link.fd,
                                        state
                                            .with_detfd(link.fd, |detfd| detfd.open_file_id())
                                            .ok(),
                                    )
                                })
                            })
                    };
                    match linked {
                        Some(reached) => Some(reached),
                        // An `O_PATH | O_NOFOLLOW` open of `/proc/self` or
                        // `/proc/thread-self` is the link itself, no
                        // directory: nothing is below it (ENOTDIR,
                        // `procfs_link_itself` below).
                        None if link_itself => None,
                        None => match procfs_selector_reached(&procfs_spelling, opener.own) {
                            // The caller's own task, as the process table
                            // identifies it now.
                            Some(selected) => {
                                let incarnation = match procfs_reached_task(&selected) {
                                    Some(task) => {
                                        let answer =
                                            crate::tool_global::procfs_task(guest, task).await;
                                        answer.exists.then(|| answer.incarnation())
                                    }
                                    None => None,
                                };
                                Some((selected, incarnation))
                            }
                            // Below a directory Detcore named: a selector's
                            // keeps its task's identity, and one the kernel
                            // named (received over SCM_RIGHTS) is named again
                            // at each use.
                            None if procfs_spelling != observed_path => {
                                let inherited = (!path.is_absolute()
                                    && call.dirfd() != libc::AT_FDCWD)
                                    .then(|| {
                                        guest
                                            .thread_state()
                                            .with_detfd(call.dirfd(), |detfd| {
                                                detfd.procfs_reached()
                                            })
                                            .ok()
                                            .flatten()
                                    })
                                    .flatten()
                                    .and_then(|(_, incarnation)| incarnation);
                                Some((procfs_spelling.clone(), inherited))
                            }
                            None => None,
                        },
                    }
                } else {
                    None
                };
                // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/4213):
                // Review binding procfs directory provenance to the object the
                // kernel opened, without sequentialized threads.
                // Without sequentialized threads another guest thread can
                // replace the descriptor (or move the working directory) that
                // a relative lookup starts from, between Detcore naming it and
                // the kernel's open. Provenance taken from the spelling then
                // describes an object the new descriptor may not hold: a
                // `/proc/self` mark on an ordinary directory would make a
                // later relative `mountinfo` render a file Linux has no way to
                // open there. So it is installed only if the descriptor the
                // kernel returned holds a directory below `/proc`, as its own
                // link names it (`descriptor_kernel_link`, one more injected
                // `readlinkat`); for `O_PATH | O_NOFOLLOW` of a selector, only
                // if it holds that selector link. With sequentialized threads
                // no other thread runs in between, and nothing is checked.
                let (reached, link_itself) = if (reached.is_some() || link_itself)
                    && !path.is_absolute()
                    && !guest.config().sequentialize_threads
                {
                    let held = Self::descriptor_kernel_link(guest, fd).await;
                    let below_proc = |name: &Path| {
                        use std::os::unix::ffi::OsStrExt;
                        name.starts_with("/proc")
                            && !name.as_os_str().as_bytes().ends_with(b" (deleted)")
                    };
                    let reached = reached.filter(|_| held.as_deref().is_some_and(below_proc));
                    let link_itself = link_itself
                        && held.as_deref().is_some_and(|name| {
                            name == Path::new("/proc/self")
                                || name == Path::new("/proc/thread-self")
                        });
                    (reached, link_itself)
                } else {
                    (reached, link_itself)
                };
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_path(&recorded_path);
                    if let Some((reached, incarnation)) = reached.clone() {
                        detfd.set_procfs_reached(reached, incarnation);
                    }
                    if link_itself {
                        detfd.set_procfs_link_itself();
                    }
                    if let Some(procfs) = procfs.clone() {
                        detfd.set_procfs(procfs);
                    }
                    if let Some(provenance) = provenance {
                        detfd.set_sigalrm_phase1_provenance(provenance);
                    }
                })?;
                resource_release_all(guest).await;
                Ok(fd as i64)
            }
            // TODO: audit for error-nondeterminism:
            Err(e) => {
                // A root-only `/proc` name outside the run's level, reached
                // through a spelling the lookup check does not resolve,
                // refuses the open before its descriptor can be named; and
                // the KVM executor refuses whatever reaches procfs.
                let e = if matches!(e, Error::Errno(Errno::EACCES | Errno::EPERM))
                    && self
                        .refused_open_reaches_procfs_name_outside_level(guest, &call)
                        .await
                {
                    Errno::ENOENT.into()
                } else {
                    e
                };
                resource_release_all(guest).await;
                Err(e)
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
        if let Ok(waited) = u32::try_from(fd)
            && let Some(refusal) = self
                .refuse_close_of_waited_listener(guest, Sysno::close, waited..=waited)
                .await
        {
            return refusal;
        }
        let channel_end = self.pipe_channel_end(guest, fd);
        let lock_tracked = self.lock_tracked_descriptions(guest, fd..=fd);
        let res = self.record_or_replay(guest, call).await;
        let fd_was_released = !matches!(res, Err(Errno::EBADF) | Err(Errno::ERESTARTSYS));
        if fd_was_released {
            if let Some(open_file_id) = guest.thread_state_mut().remove_fd(fd) {
                self.release_port_for_open_file(guest, open_file_id).await;
            }
            self.report_lock_references(guest, lock_tracked).await;
            if let Ok(fd) = u32::try_from(fd) {
                crate::tool_global::descriptors_replaced(guest, fd, fd).await;
            }
            trace!("Closed {}", fd);
            self.note_channel_end_closed(guest, channel_end).await;
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
        if first <= last
            && let Some(refusal) = self
                .refuse_close_of_waited_listener(guest, Sysno::close_range, first..=last)
                .await
        {
            return refusal;
        }

        let channel_ends = self.pipe_channel_ends_in(guest, first, last);
        let lock_tracked = match (i32::try_from(first), i32::try_from(last)) {
            (Ok(first), Ok(last)) => self.lock_tracked_descriptions(guest, first..=last),
            (Ok(first), Err(_)) => self.lock_tracked_descriptions(guest, first..=RawFd::MAX),
            _ => Vec::new(),
        };
        let result = self.record_or_replay(guest, call).await;
        if result.is_ok() {
            let released = guest.thread_state_mut().remove_fd_range(first, last);
            for open_file_id in released {
                self.release_port_for_open_file(guest, open_file_id).await;
            }
            self.report_lock_references(guest, lock_tracked).await;
            crate::tool_global::descriptors_replaced(guest, first, last).await;
            for end in channel_ends {
                self.note_channel_end_closed(guest, Some(end)).await;
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
        // A flock belongs to its open file description. Whatever the call
        // did (took, converted, released, or, refused, dropped a lock in a
        // non-atomic conversion), the description's own fdinfo now says which
        // locks it holds (`procfs::locks`).
        let fd = call.fd();
        let result = self.flock_through_the_kernel(guest, call).await;
        self.refresh_description_locks(guest, fd, LockCall::Flock)
            .await?;
        result
    }

    /// [`Self::handle_flock`]'s call itself.
    async fn flock_through_the_kernel<G: Guest<Self>>(
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
            report_flock_refusal(&format!(
                "[dtid {dettid}] blocking flock(fd={fd}, operation={operation:#x}) refused: \
                 this open file description existed before Detcore observed its lock state, \
                 so a nonblocking probe could destroy a lock that cannot be restored. \
                 {FLOCK_REFUSAL_ADVICE}"
            ));
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
        // A backend that cannot apply flock faithfully refuses it with a typed
        // refusal and no lock effect (the KVM backend refuses every lock
        // operation: https://github.com/rrnewton/reverie/issues/1069). Ask
        // before anything else is injected, since the next injection replaces
        // the backend's record. The refusal is reported as a refusal, never
        // handed to the guest as the backend's raw ENOSYS.
        let refusal = if result.is_err() {
            guest.take_unsupported_refusal()
        } else {
            None
        };
        if let Some(refusal) = refusal {
            let message = format!(
                "[dtid {dettid}] flock(fd={fd}, operation={operation:#x}) refused by the backend: {}. \
                 No lock was taken or released.",
                refusal.diagnostic
            );
            report_flock_refusal(&message);
            if !self.cfg.panic_on_unsupported_syscalls {
                crate::tool_global::report_unsupported_syscall(guest, Sysno::flock).await;
            }
            return self
                .refuse_unserviceable_operation_because(
                    guest,
                    Sysno::flock,
                    Errno::ENOLCK,
                    refusal.diagnostic,
                )
                .await;
        }

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
                report_flock_refusal(&format!(
                    "[dtid {dettid}] blocking flock(fd={fd}, operation={operation:#x}) is \
                     contended, and Detcore cannot yet park a thread on a file lock \
                     deterministically. Refusing rather than granting a lock another guest \
                     holds. {FLOCK_REFUSAL_ADVICE}"
                ));
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

        // Capture into scratch memory, never into the caller's buffer. The
        // caller receives only the sanitized bytes, so every host byte a
        // capture read left in its buffer past that length would stay visible
        // after the read returns: all of a host `/proc/modules` chunk behind
        // its empty sanitized view
        // (https://github.com/rrnewton/hermit/issues/3815). Saving and
        // restoring the caller's buffer cannot cover every destination the
        // kernel may write, such as a PROT_WRITE-only page that the save
        // cannot read. The caller's buffer sees only the sanitized
        // publication, which checks its protection as the kernel's copy does.
        let (lists_address_space, lists_locks) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.procfs_lists_address_space(),
                    detfd.procfs_lists_locks(),
                )
            })?;
        if lists_address_space {
            self.capture_procfs_on_stack(guest, call, lists_locks).await
        } else {
            self.capture_procfs_in_mapping(guest, call, lists_locks)
                .await
        }
    }

    /// Capture through a private anonymous mapping that is unmapped before
    /// returning, so no host byte stays in guest memory.
    async fn capture_procfs_in_mapping<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        lists_locks: bool,
    ) -> Result<Vec<u8>, Error> {
        const CAPTURE_CHUNK_BYTES: usize = 64 * 1024;

        let mapped = guest
            .inject_with_retry(Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(CAPTURE_CHUNK_BYTES)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await?;
        let mapping = usize::try_from(mapped)
            .ok()
            .and_then(AddrMut::<u8>::from_raw)
            .ok_or(Errno::EFAULT)?;
        let result = self
            .drain_procfs(guest, call, mapping, CAPTURE_CHUNK_BYTES, lists_locks)
            .await;
        guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Some(Addr::from(mapping).cast()))
                    .with_len(CAPTURE_CHUNK_BYTES),
            ))
            .await?;
        result
    }

    /// Capture a file that lists the address space (`maps`, `smaps`,
    /// `numa_maps`, `smaps_rollup`) through the guest stack scratch. A
    /// capture mapping would be listed in the snapshot, or merged into a
    /// neighbouring row, and then be unmapped before the guest sees it. The
    /// stack scratch changes no mapping; its original bytes are put back, so
    /// no host byte stays below the stack pointer either.
    ///
    /// The scratch below the red zone is not always there: a thread, fiber or
    /// alternate signal stack may put the stack pointer just above a guard page
    /// or the end of its mapping (compare [`Self::inject_fstat`]). The read
    /// needs no stack, so that must not fail it. The capture then goes through
    /// the caller's buffer, which this read writes anyway, with the same save
    /// and restore before the output is copied. If not even its first byte can
    /// be read and written, Linux copies nothing and the read fails `EFAULT`;
    /// it does so here without taking the snapshot.
    async fn capture_procfs_on_stack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        lists_locks: bool,
    ) -> Result<Vec<u8>, Error> {
        const CAPTURE_CHUNK_BYTES: usize = 512;

        let mut original = [0_u8; CAPTURE_CHUNK_BYTES];
        {
            let mut stack = guest.stack().await;
            let scratch = stack.reserve::<[u8; CAPTURE_CHUNK_BYTES]>().cast::<u8>();
            // `commit` writes the reserved bytes, so save them before it.
            if save_procfs_scratch(&mut guest.memory(), scratch, &mut original)
                && let Ok(guard) = stack.commit()
            {
                let result = self
                    .drain_procfs(guest, call, scratch, CAPTURE_CHUNK_BYTES, lists_locks)
                    .await;
                guest.memory().write_exact(scratch, &original)?;
                drop(guard);
                return result;
            }
        }

        let buffer = call.buf().ok_or(Errno::EFAULT)?;
        for chunk in [call.len().min(CAPTURE_CHUNK_BYTES), 1] {
            let original = &mut original[..chunk];
            if save_procfs_scratch(&mut guest.memory(), buffer, original) {
                let result = self
                    .drain_procfs(guest, call, buffer, chunk, lists_locks)
                    .await;
                guest.memory().write_exact(buffer, original)?;
                return result;
            }
        }
        Err(Errno::EFAULT.into())
    }

    /// Read the host file to EOF through `scratch`, `chunk` bytes at a time.
    ///
    /// For `/proc/locks` (`lists_locks`), each record that reads after the one
    /// whose length shows it reached the end of the kernel's lock list return
    /// is dropped, waiters and all, when its granted lock is one already read
    /// (`procfs::locks::locks_capture_without_repeats`): such records are rows
    /// the kernel prints again when the host adds a lock between two reads, at
    /// moments the host decides. Any other record is kept. Those reads are still made,
    /// through the recorder, until one returns 0, and the decision reads only
    /// their recorded lengths and bytes, so a replay drops the same bytes.
    async fn drain_procfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        scratch: AddrMut<'_, u8>,
        chunk: usize,
        lists_locks: bool,
    ) -> Result<Vec<u8>, Error> {
        const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

        let capture = call.with_buf(Some(scratch)).with_len(chunk);
        let mut contents = Vec::new();
        let mut reads = Vec::new();
        loop {
            let bytes_read = self.record_or_replay(guest, capture).await? as usize;
            if bytes_read == 0 {
                break;
            }
            if contents.len() + bytes_read > MAX_SNAPSHOT_BYTES {
                return Err(Errno::EFBIG.into());
            }
            let start = contents.len();
            contents.resize(start + bytes_read, 0);
            read_guest_exact(&guest.memory(), scratch, &mut contents[start..])?;
            reads.push(bytes_read);
        }
        if lists_locks {
            let kept =
                crate::procfs::locks::locks_capture_without_repeats(&contents, &reads, chunk);
            if kept.len() < contents.len() {
                // Not INFO: whether the host moved a lock between the reads is
                // host-timed, and INFO is compared between verified runs.
                tracing::debug!(
                    "[detcore] /proc/locks capture: dropped {} bytes of rows that reads after \
                     the end of the kernel's list printed again (reads {:?})",
                    contents.len() - kept.len(),
                    reads
                );
                return Ok(kept);
            }
        }
        Ok(contents)
    }

    /// The procfs directory that the kernel beneath the backend names for
    /// descriptor `fd`, which this guest thread just opened: its
    /// `/proc/thread-self/fd/<fd>` link, read by a `readlinkat` injected in
    /// the guest thread, which the host kernel answers under ptrace and DBT
    /// and the KVM executor answers for its own `/proc`, if that names a
    /// directory Detcore may record (`procfs_directory_link`). `None`
    /// otherwise, and where the guest stack scratch cannot hold the call's
    /// buffers, the call fails, or the link does not fit in 256 bytes: every
    /// path accepted is far shorter, and a truncated one must not match.
    async fn procfs_directory_named_for<G: Guest<Self>>(
        guest: &mut G,
        fd: RawFd,
    ) -> Option<PathBuf> {
        let named = Self::descriptor_kernel_link(guest, fd).await?;
        procfs_directory_link(&named).then_some(named)
    }

    /// The `/proc/thread-self/fd/<fd>` link of descriptor `fd`, which this
    /// guest thread holds, as the kernel beneath the backend reads it (one
    /// `readlinkat` injected in the guest thread; the host kernel under ptrace
    /// and DBT, the KVM executor for its own `/proc`). `None` where the guest
    /// stack scratch cannot hold the call's buffers, the call fails, or the
    /// link does not fit in 256 bytes.
    async fn descriptor_kernel_link<G: Guest<Self>>(guest: &mut G, fd: RawFd) -> Option<PathBuf> {
        const LINK_CAPACITY: usize = 40;
        const TARGET_CAPACITY: usize = 256;
        let mut stack = guest.stack().await;
        let link = stack.push(rendered_procfs::c_string::<LINK_CAPACITY>(
            format!("/proc/thread-self/fd/{fd}").as_bytes(),
        ));
        let target = stack.reserve::<[u8; TARGET_CAPACITY]>();
        let stack_guard = stack.commit().ok()?;
        let length = guest
            .inject(
                syscalls::Readlinkat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(link.as_raw() as *const libc::c_char))
                    .with_buf(Some(target.cast::<libc::c_char>()))
                    .with_buf_len(TARGET_CAPACITY),
            )
            .await;
        let bytes = match length.map(|length| length as usize) {
            Ok(length) if length < TARGET_CAPACITY => {
                let mut bytes = vec![0; length];
                guest
                    .memory()
                    .read_exact(target.cast::<u8>(), &mut bytes)
                    .ok()
                    .map(|()| bytes)
            }
            _ => None,
        };
        drop(stack_guard);
        Some(PathBuf::from(
            <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(bytes?),
        ))
    }

    /// What descriptor `fd` of the calling process names, as its
    /// `/proc/self/fd` link reads, through the recording: the recording reads
    /// the kernel's link and keeps its bytes, and replay, whose descriptor is
    /// an eventfd, returns the same bytes. `None` for a deleted or anonymous
    /// target, a link that cannot be read, or one too long for the 256 bytes
    /// read: every path Detcore classifies is far shorter, and a truncated
    /// one must not match.
    ///
    /// The link's name and the buffer go in the guest stack scratch, below the
    /// red zone, or, where that memory is not writable (a stack pointer just
    /// above a guard page or the lowest page of the main-thread stack), in a
    /// transient private page, as `inject_fstat` does
    /// (<https://github.com/rrnewton/hermit/issues/3328>): Detcore's
    /// bookkeeping must never fail the guest's open.
    async fn recorded_open_path<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<PathBuf>, Error> {
        const LINK_CAPACITY: usize = 32;
        const TARGET_CAPACITY: usize = 256;
        let link_name = rendered_procfs::c_string::<LINK_CAPACITY>(
            format!("/proc/thread-self/fd/{fd}").as_bytes(),
        );
        let mut stack = guest.stack().await;
        let link = stack.push(link_name);
        let target = stack.reserve::<[u8; TARGET_CAPACITY]>();
        let bytes = match stack.commit() {
            Ok(stack_guard) => {
                let bytes = self
                    .recorded_readlink(guest, link.as_raw(), target.as_raw(), TARGET_CAPACITY)
                    .await;
                drop(stack_guard);
                bytes
            }
            Err(_) => {
                info!(
                    "Guest stack scratch cannot hold the link of fd {}; using a transient page \
                     instead.",
                    fd
                );
                let len = LINK_CAPACITY + TARGET_CAPACITY;
                let mapped = guest
                    .inject_with_retry(Syscall::Mmap(
                        syscalls::Mmap::new()
                            .with_addr(None)
                            .with_len(len)
                            .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                            .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                            .with_fd(-1)
                            .with_offset(0),
                    ))
                    .await;
                let Some(page) = mapped
                    .ok()
                    .and_then(|mapped| usize::try_from(mapped).ok())
                    .and_then(AddrMut::<u8>::from_raw)
                else {
                    return Ok(None);
                };
                let bytes = if guest.memory().write_exact(page, &link_name).is_ok() {
                    self.recorded_readlink(
                        guest,
                        page.as_raw(),
                        page.as_raw() + LINK_CAPACITY,
                        TARGET_CAPACITY,
                    )
                    .await
                } else {
                    None
                };
                if let Err(errno) = guest
                    .inject_with_retry(Syscall::Munmap(
                        syscalls::Munmap::new()
                            .with_addr(Some(page.cast::<libc::c_void>().into()))
                            .with_len(len),
                    ))
                    .await
                {
                    warn!(
                        "[detcore] could not unmap the transient link page: {}",
                        errno
                    );
                }
                bytes
            }
        };
        Ok(bytes.and_then(|bytes| {
            let path = PathBuf::from(
                <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(bytes),
            );
            path.is_absolute().then_some(path)
        }))
    }

    /// `readlink(link, target, capacity)` through the recording, both at guest
    /// addresses: the bytes it read, or `None` if it failed or filled the
    /// buffer (a link that may be truncated).
    async fn recorded_readlink<G: Guest<Self>>(
        &self,
        guest: &mut G,
        link: usize,
        target: usize,
        capacity: usize,
    ) -> Option<Vec<u8>> {
        let target = AddrMut::<u8>::from_raw(target)?;
        let length = self
            .record_or_replay(
                guest,
                syscalls::Readlink::new()
                    .with_path(PathPtr::from_ptr(link as *const libc::c_char))
                    .with_buf(Some(target.cast::<libc::c_char>()))
                    .with_bufsize(capacity),
            )
            .await
            .ok()?;
        let length = usize::try_from(length)
            .ok()
            .filter(|length| *length < capacity)?;
        let mut bytes = vec![0; length];
        guest.memory().read_exact(target, &mut bytes).ok()?;
        Some(bytes)
    }

    async fn initialize_procfs_snapshot<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<(), Error> {
        let maps_target = if guest.runtime_private_memory_regions().is_some()
            && guest
                .thread_state()
                .with_detfd(call.fd(), |fd| fd.procfs_is_maps())?
        {
            if !guest.config().sequentialize_threads {
                return Err(Error::Tool(anyhow::anyhow!(
                    "runtime-private maps view unavailable: capture is not sequentialized"
                )));
            }
            let target = guest
                .thread_state()
                .with_detfd(call.fd(), |fd| fd.procfs_maps_target())?
                .ok_or_else(|| {
                    Error::Tool(anyhow::anyhow!(
                        "runtime-private maps view unavailable: opened target image is unbound"
                    ))
                })?;
            if !guest.runtime_private_memory_is_current(&target.private) {
                return Err(Error::Tool(anyhow::anyhow!(
                    "runtime-private maps view unavailable: target ownership retired"
                )));
            }
            Some(target)
        } else {
            None
        };
        let template = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_rendering_template())?;
        // A process's `mountinfo` or `mounts`, in a run with a container
        // mount table: the table's bytes, whoever opened the file.
        let mount_table = self.mount_table_snapshot(guest, call.fd())?;
        let raw_contents = match template {
            // A descriptor that cannot read (`O_PATH`, write-only, or access
            // mode 3) reads nothing (EBADF), as the kernel's read of it,
            // which a rendered snapshot does not make, would answer.
            Some(_)
                if !can_read(
                    guest
                        .thread_state()
                        .with_detfd(call.fd(), |detfd| detfd.status_flags())?,
                ) =>
            {
                return Err(Errno::EBADF.into());
            }
            Some(template) => {
                // A per-process file's `show` finds its task again: a task
                // that is gone, or whose pid another task now holds, is ESRCH.
                // At position 0 only: a description that has no snapshot at
                // another position is one rebuilt from a placeholder whose
                // first read happened in another process
                // (`adopt_received_placeholder`, `adopt_placeholder`), where
                // Linux reads on from the buffer that read filled, without
                // looking for the task.
                let position = guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| detfd.procfs_position())?
                    .map_or(0, |(offset, _)| offset);
                if position == 0 {
                    self.require_rendered_task(guest, call.fd()).await?;
                }
                mount_table.unwrap_or_else(|| template.to_vec())
            }
            // A mount table file the kernel opened takes no capture of the
            // kernel's bytes, which the table replaces; a descriptor that
            // cannot read reads nothing (EBADF), as the kernel's read of it
            // would answer.
            None if let Some(table) = mount_table => {
                if !can_read(
                    guest
                        .thread_state()
                        .with_detfd(call.fd(), |detfd| detfd.status_flags())?,
                ) {
                    return Err(Errno::EBADF.into());
                }
                table
            }
            None => self.snapshot_procfs(guest, call).await?,
        };
        let raw_contents = if let Some(target) = maps_target {
            if !guest.runtime_private_memory_is_current(&target.private) {
                return Err(Error::Tool(anyhow::anyhow!(
                    "runtime-private maps view unavailable: target ownership changed during capture"
                )));
            }
            crate::procfs::private_maps::project(&raw_contents, &target.private.regions).map_err(
                |reason| {
                    Error::Tool(anyhow::anyhow!(
                        "runtime-private maps view unavailable: {reason}"
                    ))
                },
            )?
        } else {
            raw_contents
        };
        // The guest mount view is the launch namespace (guest mount/unshare/
        // setns are refused under Detcore). Ephemeral host FUSE seed rows
        // (per-process and named-tool seeds) are other tenants' propagated
        // runtime state, not part of that namespace; exclude the class at
        // capture so snapshot identity assignment and every later read agree
        // on the same membership.
        let contents = if guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_mountinfo_identities())?
        {
            crate::procfs::exclude_ephemeral_host_seed_mounts(&raw_contents)
        } else {
            raw_contents
        };
        // Another process's `stat` or `status`, where the kernel numbers
        // processes otherwise than Detcore (DBT), is renumbered to the
        // guest's numbers (slice B3a-4 of
        // https://github.com/rrnewton/hermit/issues/4266).
        let held = guest.thread_state().with_detfd(call.fd(), |detfd| {
            detfd.procfs_kernel_numbers_held(&contents)
        })?;
        // The init's files (`/proc/1/...`, Hermit's own process and threads)
        // keep the numbers the kernel gives them, as their paths do. Which
        // process's file it is, the kernel's name for the descriptor says
        // where the tool shares the guest's descriptors (DBT), whatever path
        // reached it.
        let path = if guest.config().backend.tool_shares_guest_descriptor_table {
            std::fs::read_link(format!("/proc/thread-self/fd/{}", call.fd())).ok()
        } else {
            guest
                .thread_state()
                .with_detfd(call.fd(), |detfd| detfd.path())?
        };
        let init = path.is_some_and(|path| crate::procfs::ids::names_init(&path));
        let held = if init { Vec::new() } else { held };
        if let Some(numbers) = Self::guest_procfs_numbers(guest, &held).await {
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                detfd.set_procfs_guest_numbers(numbers.clone())
            })?;
        }
        let virtual_uptime_seconds = self.calculate_procfs_uptime(guest).await?;
        // A file Detcore renders at its first read: at the uptime an earlier
        // read of the same open file took its snapshot at, which a process
        // without Detcore's state of it (after `exec` or `fork` where the
        // tool runs in each guest process) finds on the placeholder.
        let virtual_uptime_seconds = match guest.thread_state().with_detfd(call.fd(), |detfd| {
            detfd
                .procfs_rendered_at_first_read()
                .then(|| detfd.procfs_rendered_path())
                .flatten()
        })? {
            Some(path) => {
                self.first_read_uptime(guest, call.fd(), path, virtual_uptime_seconds)
                    .await
            }
            None => virtual_uptime_seconds,
        };
        // Only `/proc/stat` renders btime. Computing it for every snapshot let
        // an unrepresentable boot instant refuse unrelated files such as
        // `/proc/uptime`.
        let needs_boot_time = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_boot_time())?;
        let virtual_boot_time_seconds = needs_boot_time
            .then(|| self.calculate_procfs_boot_time())
            .transpose()?;
        let virtual_realtime_seconds = i64::try_from(thread_observe_time(guest).await.as_secs())
            .map_err(|_| Errno::EOVERFLOW)?;
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
        // The kernel's fdinfo for a procfs file Detcore renders describes the
        // memfd standing in for it. Linux's names the file: Detcore's cursor,
        // the guest's procfs mount and the file's inode. The cursor is the one
        // processes sharing the open file share.
        if let Some(target_fd) = target_fd {
            self.adopt_rendered_procfs_position(guest, target_fd)
                .await?;
        }
        let rendered_target = match target_fd {
            Some(target_fd) => guest
                .thread_state()
                .with_detfd(target_fd, |detfd| {
                    detfd.procfs_rendered_path().map(|path| {
                        (
                            path,
                            detfd.procfs_position().map_or(0, |(offset, _)| offset),
                        )
                    })
                })
                .ok()
                .flatten(),
            None => None,
        };
        let contents = match rendered_target {
            Some((_, offset)) => format!(
                "pos:\t{offset}\nflags:\t0\nmnt_id:\t{}\nino:\t0\n",
                rendered_procfs::guest_procfs_mount_id()?
            )
            .into_bytes(),
            None => contents,
        };
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
            let stat = match cached_stat {
                Some(stat) => stat,
                None => DetStat::from(self.inject_fstat(guest, target_fd).await?),
            };
            // Keyed as `fstat` keys the descriptor, so the two numbers agree.
            let virtual_inode = match inode_override {
                Some(inode) => inode,
                // Detcore numbered it when it rendered the file.
                None if let Some((path, _)) = rendered_target => {
                    DetInode::mint(procfs_inode::rendered_inode(path))
                }
                None => {
                    let procfs =
                        Self::procfs_entry(guest, &stat, StatTarget::Descriptor(target_fd)).await;
                    let files = self.rendered_files(guest);
                    if let Some(path) = rendered_entry(&procfs, &stat, guest.config(), files) {
                        DetInode::mint(procfs_inode::rendered_inode(path))
                    } else {
                        determinize_inode_observing_mtime(
                            guest,
                            stat.raw_inode(),
                            procfs.name(stat.dev),
                            ObservedMtime::Unobserved,
                            InodeSighting::Descriptor,
                        )
                        .await
                        .0
                    }
                }
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
                // Same guest mount model as the mountinfo capture path:
                // ephemeral host seed rows are not guest namespace members
                // and must not shift run-global mount-ID assignment.
                let mountinfo_contents =
                    crate::procfs::exclude_ephemeral_host_seed_mounts(&mountinfo_contents);
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
                // A file on a mount the container mount table hides reports
                // its nearest shown ancestor (`ContainerMountTable::select`).
                let raw_mount_id = self.shown_mount_id(guest, raw_mount_id);
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
        // The guest's CPUID instruction is answered from Detcore's table
        // exactly when CPUID is virtualized and the backend delivers the
        // instruction to Detcore (`handle_cpuid_event`), on ptrace and KVM
        // alike; without CPUID interception the guest sees the host's CPU,
        // and so does this file. There Detcore renders the file itself
        // (`ProcfsFile::rendered_path`); this is for a /proc/cpuinfo the
        // kernel opened: through a symbolic link, after `chroot`, or under
        // record and replay.
        // A placeholder (`procfs_rendered_path`), which exists only where the
        // session renders the file, is rendered from the table: it holds no
        // other bytes.
        let (is_cpuinfo, placeholder) = guest.thread_state().with_detfd(call.fd(), |detfd| {
            (
                detfd.procfs_is_cpuinfo(),
                detfd.procfs_rendered_path().is_some(),
            )
        })?;
        let virtual_cpuinfo = (is_cpuinfo && (placeholder || self.rendered_files(guest).cpuinfo))
            .then(|| self.virtual_cpuid().cpuinfo());
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
            //
            // The lowest descriptor wins a shared identity, as in
            // `deterministic_stdio_inode_for_raw`. Detcore gives all three
            // inherited descriptors the stat of its own stdin (`setup_stdio`),
            // so they share one until the guest replaces them; a mapping of
            // that file is a mapping of fd 0, and must report the 1000 that
            // fd 0's fdinfo reports, not stderr's 1002.
            let mut stdio_by_raw_inode: BTreeMap<RawInode, DetInode> = BTreeMap::new();
            for fd in libc::STDIN_FILENO..=libc::STDERR_FILENO {
                let cached = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| {
                        let inode = deterministic_stdio_inode_for_resource(fd, detfd.resource())?;
                        detfd.stat().map(|stat| (stat.raw_inode(), inode))
                    })
                    .ok()
                    .flatten();
                if let Some((raw, det)) = cached {
                    stdio_by_raw_inode.entry(raw).or_insert(det);
                }
            }
            // One numbering request per file-backed line, in line order
            // (address order, which the guest's own mmaps decide), repeats and
            // stdio-backed lines included. The pool numbers files in request
            // order and every request consumes a number, so sorting by raw
            // `(dev, ino)` would let a host inode decide which file gets which
            // number, and skipping lines whose host pair repeats (hard-linked
            // libraries) or matches stdio would let host identity decide how
            // many numbers a read consumes
            // (https://github.com/rrnewton/hermit/issues/2897).
            //
            // The device a mapping names is the filesystem's own, which on
            // btrfs is not the per-subvolume device `stat` reports; the pair is
            // the identity either way, as it is for the device determinized
            // below.
            let raw_pairs: Vec<(u64, u64)> = String::from_utf8_lossy(&contents)
                .lines()
                .filter_map(crate::procfs::mapping_header_identity)
                .collect();
            let pooled = determinize_mapping_inodes(
                guest,
                raw_pairs
                    .iter()
                    .map(|&(raw_dev, raw_inode)| RawInode::new(raw_dev, raw_inode))
                    .collect(),
            )
            .await;
            for (&(raw_dev, raw_inode), pooled) in raw_pairs.iter().zip(pooled) {
                if mapping_identities.contains_key(&(raw_dev, raw_inode)) {
                    continue;
                }
                let det_inode = stdio_by_raw_inode
                    .get(&RawInode::new(raw_dev, raw_inode))
                    .copied()
                    .unwrap_or(pooled);
                // The device pool numbers first sightings only, so a repeated
                // device request changes nothing.
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
            let mut snapshot = MountInfoSnapshot::new(
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
            // Number this view's mount IDs through the same run-global pool
            // as fdinfo. IDs the run has already shown keep their numbers;
            // the rest are numbered in the order this view presents them
            // (rows, then parent-only IDs). The canonical order, which may
            // follow the captured namespace, only validates the view.
            let observation_order = snapshot.raw_mount_id_observation_order();
            let Some(virtual_ids) = resolve_mountinfo_identities(
                guest,
                snapshot.raw_mount_id_order(),
                observation_order.clone(),
            )
            .await
            else {
                return Err(Error::Tool(anyhow::anyhow!(
                    "mountinfo mount-ID order changed after the run-global identity snapshot"
                )));
            };
            if !snapshot.use_run_mount_ids(&observation_order, &virtual_ids) {
                return Err(Error::Tool(anyhow::anyhow!(
                    "run-global mount identities do not cover this mountinfo snapshot"
                )));
            }
            Some(snapshot)
        } else {
            None
        };
        // `/proc/locks` keeps the guest's own rows only: an OFD lock has no
        // owning task, so Linux shows every process's OFD locks from every
        // PID namespace (`procfs::locks`). Decided before the sanitizer
        // numbers identities, so a dropped row shifts no remaining id. The
        // capture above and this decision are two steps of one turn: with
        // sequentialized threads no other guest thread's lock call (and its
        // fdinfo record) runs between them. Parallel execution would need
        // them taken as one.
        let lock_rows_kept = if guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_lists_locks())?
        {
            match crate::procfs::locks::lock_rows(&contents) {
                Some(rows) => match crate::tool_global::guest_lock_rows(guest, rows).await {
                    Ok(kept) => Some(kept),
                    Err(reason) => {
                        let reason = format!(
                            "/proc/locks: {reason}; that description's rows cannot be told \
                             from another process's"
                        );
                        self.refuse_lock_bookkeeping(guest, reason).await
                    }
                },
                // Rows that do not parse: the sanitizer refuses the snapshot.
                None => None,
            }
        } else {
            None
        };
        let machine = crate::machine::MachineDescription::of(guest.config());
        // A status reports the guest's own mask of a thread Detcore holds
        // under a private blocking mask, which that thread published to the
        // scheduler; the kernel's `SigBlk` is Detcore's mask (Codex review of
        // R2b-2 stage 2, P2; https://github.com/rrnewton/hermit/issues/4273).
        // Masks are published only where the scheduler models signal targets.
        let mask_thread = guest.thread_state().with_detfd(call.fd(), |detfd| {
            detfd.procfs_status_mask_thread(&contents)
        })?;
        let published_sigblk = match mask_thread {
            Some(thread) if crate::scheduler::discarded_at_send::records(guest.config()) => {
                crate::tool_global::published_guest_sigmask(guest, DetTid::from_raw(thread)).await
            }
            _ => None,
        };
        guest.thread_state().with_detfd(call.fd(), |detfd| {
            detfd.initialize_procfs(
                contents.clone(),
                ProcfsSnapshotContext {
                    mapping_identities: mapping_identities.clone(),
                    mountinfo: mountinfo.clone(),
                    virtual_uptime_seconds,
                    virtual_boot_time_seconds,
                    virtual_realtime_seconds,
                    virtual_pid,
                    virtual_ppid,
                    virtual_pty_count,
                    fdinfo_identity,
                    random_uuid,
                    virtual_cpuinfo: virtual_cpuinfo.clone(),
                    machine: machine.clone(),
                    published_sigblk,
                    lock_rows_kept: lock_rows_kept.clone(),
                },
            );
        })?;
        Ok(())
    }

    /// The bytes of the procfs file open as `fd`, if it is a process's
    /// `mountinfo` or `mounts` and the run has a container mount table
    /// (`Config::container_mount_table`): the table's `mountinfo`, with raw
    /// identities that the snapshot then numbers as it numbers the kernel's,
    /// or its `mounts`. Wherever the file was opened from: rendered by Detcore,
    /// or opened by the kernel (under record and replay, through a spelling
    /// Detcore does not resolve, on a backend's own procfs), except one the
    /// kernel opened after the process it names changed its root, whose view
    /// the kernel decides (`chroot` keeps the kernel's path, as for every
    /// rendered file). `None` for every other file.
    fn mount_table_snapshot<G: Guest<Self>>(
        &self,
        guest: &G,
        fd: RawFd,
    ) -> Result<Option<Vec<u8>>, Errno> {
        let Some(table) = guest.config().container_mount_table.as_ref() else {
            return Ok(None);
        };
        let (rendering, rendered, pinned) = guest.thread_state().with_detfd(fd, |detfd| {
            (
                detfd.procfs_mount_table_rendering(),
                detfd.procfs_rendered_path().is_some(),
                detfd.procfs_pinned_mount_view(),
            )
        })?;
        let Some(rendering) = rendering else {
            return Ok(None);
        };
        // A rendered file is the table, as its open decided. A file the kernel
        // opened reads the view its open pinned; one nothing pinned (a
        // descriptor Detcore learned of otherwise), the reader's.
        let kernel_view = !rendered
            && pinned.unwrap_or_else(|| {
                guest
                    .thread_state()
                    .changed_root
                    .load(std::sync::atomic::Ordering::SeqCst)
            });
        if kernel_view {
            return Ok(None);
        }
        Ok(Some(match rendering {
            crate::procfs::MountTableRendering::Mountinfo => table.mountinfo(),
            crate::procfs::MountTableRendering::Mounts => {
                table.mounts(&guest.config().mountinfo_root_rewrites)
            }
        }))
    }

    /// Whether the process's `mountinfo` or `mounts` the kernel opened as
    /// `file` reads the kernel's view: the task the path names has a changed
    /// root (`tool_global::root_changed`, which follows filesystem groups
    /// shared through CLONE_FS), or Detcore cannot tell which task it names.
    /// As on Linux, where `mounts_open_common` takes the target task's root,
    /// the reader's own root does not decide it. The kernel's view is
    /// Linux's own answer for whatever task the path names, so it is the
    /// answer whenever the task is unknown: never the launch table.
    async fn mount_view_is_kernel<G: Guest<Self>>(&self, guest: &mut G, file: &ProcfsFile) -> bool {
        // In a nested PID namespace the path's numbers are that namespace's
        // (a child that kept a `/proc` directory descriptor across
        // CLONE_NEWPID and opens `self/mountinfo` through it, for one), and
        // Detcore does not map them here.
        let Some((pid, tid)) = self.procfs_caller_ids(guest) else {
            return true;
        };
        let mut bound = file.clone();
        bound.bind_task(pid, tid);
        match bound.bound_task() {
            Some(task) => {
                crate::tool_global::root_changed(
                    guest,
                    detcore_model::pid::DetPid::from_raw(task.thread.unwrap_or(task.process)),
                )
                .await
            }
            None => true,
        }
    }

    /// The raw mount ID a file on mount `raw_mount_id` reports to the guest:
    /// with a container mount table, and until the process changes its root,
    /// a hidden mount's nearest shown ancestor
    /// (`ContainerMountTable::shown_mount_id`); otherwise the mount itself.
    pub(crate) fn shown_mount_id<G: Guest<Self>>(&self, guest: &G, raw_mount_id: u64) -> u64 {
        match guest.config().container_mount_table.as_ref() {
            Some(table)
                if !guest
                    .thread_state()
                    .changed_root
                    .load(std::sync::atomic::Ordering::SeqCst) =>
            {
                table.shown_mount_id(raw_mount_id)
            }
            _ => raw_mount_id,
        }
    }

    /// Write `stat`, a procfs file Detcore renders, as `statx`'s result, with
    /// the guest's procfs mount's ID, as its fdinfo `mnt_id` shows it.
    async fn write_rendered_statx<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Statx,
        stat: DetStat,
    ) -> Result<i64, Error> {
        let statptr = call.statx().ok_or(Errno::EFAULT)?;
        let mut statx: reverie::syscalls::linux_abi::Statx = stat.into();
        let raw_mount_id = rendered_procfs::guest_procfs_mount_id().ok();
        let unique = statx_requested_mask(call) & STATX_MNT_ID_UNIQUE != 0;
        self.fill_statx_mount_id(guest, &mut statx, raw_mount_id, unique)
            .await;
        guest.memory().write_value(statptr.0, &statx)?;
        Ok(0)
    }

    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/4213):
    // Review STATX_MNT_ID numbered as fdinfo's mnt_id, a hidden mount's file
    // reporting its nearest shown ancestor.
    /// Fill `statx`'s mount ID for a file on mount `raw_mount_id` (a classic
    /// ID), as Linux fills it whether or not the caller asked for it
    /// (`vfs_statx_path`, v6.12 fs/stat.c:248-259): the number its fdinfo
    /// `mnt_id` and its `mountinfo` row show, through the run's mount-ID
    /// pool, a hidden mount's file showing its nearest shown ancestor's
    /// ([`Self::shown_mount_id`]), with `STATX_MNT_ID`. If the caller asked
    /// for `STATX_MNT_ID_UNIQUE` (`unique`), Linux reports the mount's unique
    /// ID and that bit instead; here that is the same number plus
    /// [`MNT_UNIQUE_ID_OFFSET`], which is above every classic ID, the same
    /// for every file of the mount, different for every other mount, and
    /// the first valid unique ID for the first mount numbered. Where there is
    /// no such number (no mount provenance to number it by, outside Hermit's
    /// container, or no mount ID from the kernel), the field is 0 and both
    /// mask bits are cleared, so no caller reads a number that names nothing.
    ///
    /// Where this differs from Linux: the pool is keyed by classic IDs, which
    /// Linux can give a new mount after an unmount (an IDA), while a unique
    /// ID is never reused. A mount made after another's unmount, on the
    /// freed classic ID, shows that mount's unique ID here too.
    async fn fill_statx_mount_id<G: Guest<Self>>(
        &self,
        guest: &mut G,
        statx: &mut reverie::syscalls::linux_abi::Statx,
        raw_mount_id: Option<u64>,
        unique: bool,
    ) {
        let guest_mount_id = match raw_mount_id {
            Some(raw_mount_id) if guest.config().mountinfo_mount_ids_captured => {
                let raw_mount_id = self.shown_mount_id(guest, raw_mount_id);
                determinize_mount_id(guest, raw_mount_id, None).await
            }
            _ => None,
        };
        let reported = match guest_mount_id {
            Some(id) if unique => id
                .checked_add(MNT_UNIQUE_ID_OFFSET)
                .map(|id| (id, STATX_MNT_ID_UNIQUE)),
            Some(id) => Some((id, reverie::syscalls::linux_abi::STATX_MNT_ID)),
            None => None,
        };
        statx.stx_mask &= !(reverie::syscalls::linux_abi::STATX_MNT_ID | STATX_MNT_ID_UNIQUE);
        match reported {
            Some((id, bit)) => {
                statx.stx_mnt_id = id;
                statx.stx_mask |= bit;
            }
            None => statx.stx_mnt_id = 0,
        }
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
            if let Ok(Some(status_flags)) = guest.thread_state().with_detfd(call.fd(), |detfd| {
                (detfd.ty() == FdType::Rng).then(|| detfd.status_flags())
            }) {
                // Replay reserves random-device fd slots with eventfds. A
                // physical zero-length read of that placeholder returns EINVAL
                // even though the logical random-device read must return zero.
                require_random_device_read_access(status_flags)?;
                let limit =
                    crate::iovecs::UserAddressLimit::from_query(guest.user_address_limit())?;
                // vfs_read still checks access_ok for a zero-length buffer:
                // NULL is valid, but an address beyond TASK_SIZE is EFAULT.
                limit.validate(&[crate::iovecs::ImportedIovec {
                    base: call.buf().map_or(0, |address| address.as_raw()),
                    len: 0,
                }])?;
                return Ok(0);
            }
            // A zero-count read transfers nothing on a file or stream, but on a
            // datagram or SEQPACKET socket it consumes a pending message. Its
            // result goes through record/replay: a replayed descriptor may be
            // a placeholder whose own zero-count read fails differently.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-3601)
            return self
                .record_or_replay_preserving_tool_errors(guest, call)
                .await;
        }

        let (needs_procfs_snapshot, serves_procfs_snapshot, cannot_read) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.procfs_needs_snapshot(),
                    detfd.procfs_serves_snapshot(),
                    !can_read(detfd.status_flags()),
                )
            })?;
        // A descriptor that cannot read (`O_PATH`, write-only, access mode 3)
        // reads nothing (EBADF), before its buffer is looked at, as Linux's
        // `vfs_read` checks, whether or not a rendered file's snapshot is
        // taken yet (`/proc/uptime`'s is taken at its first read).
        if serves_procfs_snapshot && cannot_read {
            return Err(Errno::EBADF.into());
        }
        if serves_procfs_snapshot {
            validate_procfs_destination(
                || crate::iovecs::UserAddressLimit::from_query(guest.user_address_limit()),
                call.buf(),
                call.len(),
            )?;
        }
        if needs_procfs_snapshot {
            self.initialize_procfs_snapshot(guest, call).await?;
        }

        self.adopt_rendered_procfs_position(guest, call.fd())
            .await?;
        // Like a seq_file read, advance the shared cursor only by the bytes
        // actually copied: a read that faults before copying anything leaves
        // it where it was.
        let procfs_preview = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.preview_procfs(call.len()))?;
        if let Some((offset, bytes)) = procfs_preview {
            let faults_on_a_short_copy = guest
                .thread_state()
                .with_detfd(call.fd(), |detfd| detfd.procfs_faults_on_a_short_copy())?;
            let copied = copy_procfs_read(
                &mut guest.memory(),
                call.buf(),
                &bytes,
                faults_on_a_short_copy,
            )?;
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                detfd.commit_procfs_read(offset, copied);
            })?;
            self.sync_rendered_procfs_position(guest, call.fd()).await?;
            return Ok(copied as i64);
        }

        self.fifo_polls_if_a_guest_holds_its_peer(guest, call.fd(), FifoPeer::Writer)
            .await;
        let (fd_type, physically_nonblocking, logically_nonblocking, resource, descriptor) =
            guest.thread_state_mut().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.physically_nonblocking(),
                    detfd.is_nonblocking(),
                    detfd.resource(),
                    detfd.clone(),
                )
            })?;

        if let Some(resource) = resource {
            let mut request = guest.thread_state().mk_request(resource, Permission::R);
            if should_tag_host_timed_internal_pipe_io(
                guest.config().backend.internal_pipe_turns_are_host_timed,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(HOST_TIMED_INTERNAL_PIPE_IO_FYI);
            }
            resource_request(guest, request).await;
        }

        let res = match fd_type {
            FdType::Rng => {
                trace!("Read call RNG fd {}, simulating...", call.fd());
                let status_flags = descriptor.status_flags();
                descriptor
                    .with_random_device_stream(|offset| {
                        require_random_device_read_access(status_flags)?;
                        let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
                        self.fill_random_device_bytes(guest, remote_buf, call.len(), offset)
                    })
                    .map(|n| n as i64)
            }
            FdType::Regular | FdType::Pipe if descriptor.inherited_external_read().is_some() => {
                // An authenticated controller input or terminal can block
                // after this Device grant.
                // Announce one external kernel read even when our cached flags
                // say nonblocking: a fork/exec peer can change the shared OFD's
                // flags while the process-local model remains stale (the
                // pre-existing https://github.com/rrnewton/hermit/issues/3520).
                // The kernel keeps its actual O_NONBLOCK semantics, including
                // immediate EAGAIN. Never fill an external short read to len.
                self.record_or_replay_blocking(guest, call.into()).await
            }
            FdType::Regular => {
                if guest.config().deterministic_io {
                    self.deterministic_read(guest, call).await
                } else {
                    Ok(self.record_or_replay(guest, call).await?)
                }
            }
            FdType::Signalfd
                if call.len() >= 2 * crate::syscalls::signal::SIGNALFD_RECORD
                    && self.signalfd_reads_per_record(guest, call.fd()).await =>
            {
                self.read_signalfd_per_record(guest, call).await
            }
            FdType::Signalfd => self.read_signalfd(guest, call, true).await,
            FdType::Timerfd => {
                trace!("Possibly blocking read call on timerfd {}", call.fd());
                // Expirations up to this thread's virtual now make the host
                // descriptor readable before a nonblocking read's one attempt;
                // a blocking read's retries publish again at each turn.
                crate::syscalls::timerfd::publish_expired(guest).await;
                let result = self.execute_nonblockable_fd_syscall(guest, call).await;
                crate::syscalls::timerfd::finish_read(guest, call.fd(), call.buf(), result)
            }
            FdType::Eventfd | FdType::Inotify => {
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
        // O2: a signalfd read (even one that faulted copying out a record
        // it had already dequeued) may have taken the process's ITIMER_REAL
        // copy (https://github.com/rrnewton/hermit/issues/4139).
        if fd_type == FdType::Signalfd {
            crate::tool_global::reconcile_alarm(guest).await;
        }
        resource_release_all(guest).await;
        res
    }

    /// One read of signalfd `call`, as the kernel answers it, with each
    /// record put through the child-exit ledger and the discarded-at-send
    /// record (`reconcile_signalfd_records`). A read whose every record was
    /// dropped reads again if `may_read_again`, as if those copies had never
    /// been queued; otherwise it answers 0.
    async fn read_signalfd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        may_read_again: bool,
    ) -> Result<i64, Error> {
        loop {
            trace!("Possibly blocking read call on signalfd {}", call.fd());
            let extents = [(call.buf().map_or(0, |buf| buf.as_raw()), call.len())];
            let before =
                crate::syscalls::signal::capture_signalfd_buffer(guest, &extents, call.len());
            let result = self.execute_nonblockable_fd_syscall(guest, call).await;
            // A SIGCHLD read here goes through the child-exit ledger as
            // one delivered to a handler does. A read whose every record
            // was a dropped duplicate reads again, as if those copies had
            // never been queued.
            match (result, call.buf()) {
                (Ok(read), Some(buf))
                    if read > 0 && self.cfg.backend.reports_child_exit_publication =>
                {
                    let Some(kept) = self
                        .reconcile_signalfd_records(guest, buf, read as usize, &before)
                        .await
                    else {
                        break self
                            .refuse_unrestorable_signalfd_record(guest, Sysno::read)
                            .await;
                    };
                    if self
                        .settle_faulted_signalfd_record(
                            guest,
                            call.fd(),
                            &extents,
                            Some(read as usize),
                        )
                        .await
                    {
                        break self
                            .refuse_faulted_signalfd_discard(guest, Sysno::read)
                            .await;
                    }
                    if kept > 0 {
                        break Ok(kept as i64);
                    }
                    if !may_read_again {
                        break Ok(0);
                    }
                }
                (Err(Error::Errno(Errno::EFAULT)), _) => {
                    if self
                        .settle_faulted_signalfd_record(guest, call.fd(), &extents, None)
                        .await
                    {
                        break self
                            .refuse_faulted_signalfd_discard(guest, Sysno::read)
                            .await;
                    }
                    break Err(Errno::EFAULT.into());
                }
                (result, _) => break result,
            }
        }
    }

    /// Whether a signalfd read of `fd` takes one record per kernel read
    /// (4262 design §4.4): some signal it covers has an entry behind the
    /// front of its list, or a stale timer entry, in the calling thread's or
    /// its process's queue. A read must then dequeue one record at a time:
    /// sending a list's next entry before the next record, as Linux's
    /// `signalfd_dequeue` loop takes them in its order, and refilling the slot
    /// of a stale record it drops, as Linux skips such an entry inside the
    /// dequeue. With only single valid entries, every entry is physically
    /// queued and none is dropped, so one kernel read returns what Linux
    /// returns, in count and in order.
    async fn signalfd_reads_per_record<G: Guest<Self>>(&self, guest: &mut G, fd: i32) -> bool {
        use crate::scheduler::discarded_at_send::DiscardedSignalAnswer;
        use crate::scheduler::discarded_at_send::DiscardedSignalControl;
        if !crate::scheduler::discarded_at_send::records(guest.config()) {
            return false;
        }
        let Some(mask) = crate::syscalls::signal::signalfd_mask(guest.pid().as_raw(), fd) else {
            return false;
        };
        matches!(
            crate::tool_global::discarded_signal(guest, DiscardedSignalControl::Queued).await,
            DiscardedSignalAnswer::Bits { thread, process } if (thread | process) & mask != 0
        )
    }

    /// A signalfd read taken one record at a time
    /// (`signalfd_reads_per_record`): the first record's read may block, as
    /// the call would; each later one is read only while a signal the
    /// descriptor covers is pending, so it never blocks (fs/signalfd.c reads
    /// later records without waiting). A fault after k records returns
    /// k records, as the kernel's copy-out does.
    async fn read_signalfd_per_record<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<i64, Error> {
        const RECORD: usize = crate::syscalls::signal::SIGNALFD_RECORD;
        let Some(buf) = call.buf() else {
            return self.read_signalfd(guest, call, true).await;
        };
        let mask = crate::syscalls::signal::signalfd_mask(guest.pid().as_raw(), call.fd());
        let mut done = 0usize;
        // A read whose records were all dropped (stale timer entries) frees
        // its slot. Each such read dequeued a physical record and popped its
        // entry, and the lists are finite, so the refills end without a cap
        // (the Codex re-review of
        // https://github.com/rrnewton/hermit/pull/4491, P2: 80 stale entries).
        while done + RECORD <= call.len() {
            if done > 0 {
                let pending =
                    crate::syscalls::threads::read_kernel_signal_state(guest.pid(), guest.tid())
                        .map(|state| state.thread_pending | state.shared_pending)
                        .unwrap_or(0);
                if mask.is_none_or(|mask| pending & mask == 0) {
                    break;
                }
            }
            // A guest address, never dereferenced here.
            let at = AddrMut::<u8>::from_raw(buf.as_raw() + done);
            let record = syscalls::Read::new()
                .with_fd(call.fd())
                .with_buf(at)
                .with_len(RECORD);
            match self.read_signalfd(guest, record, done == 0).await {
                Ok(read) if read > 0 => done += read as usize,
                Ok(read) if done == 0 => return Ok(read),
                // Linux skips a stale entry inside the dequeue and fills the
                // slot with the next pending signal (the Claude review of
                // https://github.com/rrnewton/hermit/pull/4491, P2-1).
                Ok(_) => {}
                Err(error) if done == 0 => return Err(error),
                Err(_) => break,
            }
        }
        Ok(done as i64)
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
            // As for read, a zero-count pread's result goes through
            // record/replay.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-3601)
            return self
                .record_or_replay_preserving_tool_errors(guest, call)
                .await;
        }

        let offset = usize::try_from(call.offset()).map_err(|_| Errno::EINVAL)?;
        let (needs_procfs_snapshot, serves_procfs_snapshot, cannot_read) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.procfs_needs_snapshot(),
                    detfd.procfs_serves_snapshot(),
                    !can_read(detfd.status_flags()),
                )
            })?;
        // A descriptor that cannot read (`O_PATH`, write-only, access mode 3)
        // reads nothing (EBADF), before its buffer is looked at, as Linux's
        // `vfs_read` checks, whether or not a rendered file's snapshot is
        // taken yet (`/proc/uptime`'s is taken at its first read).
        if serves_procfs_snapshot && cannot_read {
            return Err(Errno::EBADF.into());
        }
        if serves_procfs_snapshot {
            validate_procfs_destination(
                || crate::iovecs::UserAddressLimit::from_query(guest.user_address_limit()),
                call.buf(),
                call.len(),
            )?;
        }
        if needs_procfs_snapshot {
            let read = syscalls::Read::new()
                .with_fd(call.fd())
                .with_buf(call.buf())
                .with_len(call.len());
            self.initialize_procfs_snapshot(guest, read).await?;
        } else if serves_procfs_snapshot
            && guest.thread_state().with_detfd(call.fd(), |detfd| {
                detfd
                    .procfs_position()
                    .is_some_and(|(position, _)| position != offset)
            })?
        {
            // A read at a position the buffered snapshot does not start at
            // runs a per-process file's `show` again (`traverse`).
            self.require_rendered_task(guest, call.fd()).await?;
        }

        let procfs_bytes = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.take_procfs_at(offset, call.len()))?;
        if let Some(bytes) = procfs_bytes {
            let faults_on_a_short_copy = guest
                .thread_state()
                .with_detfd(call.fd(), |detfd| detfd.procfs_faults_on_a_short_copy())?;
            let copied = copy_procfs_read(
                &mut guest.memory(),
                call.buf(),
                &bytes,
                faults_on_a_short_copy,
            )?;
            return Ok(copied as i64);
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

    /// Whether `fd`, whose open recorded the host stat `cached`, is a
    /// directory that is not on procfs (see [`Self::handle_lseek`]). procfs
    /// keeps the host's seek behavior, as it keeps the host's sizes. A
    /// descriptor's type and device never change, so the stat its open took
    /// answers without another `fstat` (Claude re-check of
    /// https://github.com/rrnewton/hermit/pull/4369). Its block size can (a
    /// FUSE server reports it), so it only rules procfs out: the kernel's
    /// procfs always has 1024-byte blocks, and a directory that may be procfs
    /// is proved so by naming its entry, as a stat does, and KVM's by its
    /// device ([`procfs_inode::is_backend_procfs_device`]). Under DBT, where
    /// Detcore names no entries, one with procfs's stat fields keeps the
    /// host's seek unproved ([`procfs_inode::unprovable_procfs_signature`]).
    /// With metadata virtualized every descriptor the guest opened has a stat;
    /// those without are Detcore's own pipes and sockets.
    async fn is_seekable_directory<G: Guest<Self>>(
        guest: &mut G,
        fd: RawFd,
        cached: Option<DetStat>,
    ) -> bool {
        let Some(stat) = cached.filter(|stat| stat.mode & libc::S_IFMT == libc::S_IFDIR) else {
            return false;
        };
        if procfs_inode::is_backend_procfs_device(&guest.config().backend, stat.dev, stat.blksize)
            || procfs_inode::unprovable_procfs_signature(
                &guest.config().backend,
                guest.thread_state().physical_tid,
                stat.dev,
                stat.blksize,
            )
        {
            return false;
        }
        !Self::procfs_entry(guest, &stat, StatTarget::Descriptor(fd))
            .await
            .is_procfs()
    }

    /// SYS_lseek system call.
    pub async fn handle_lseek<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Lseek,
    ) -> Result<i64, Error> {
        let timer_slack_binding = self.timer_slack_binding(guest, call.fd())?;
        // A seek must not move the position under a `getdents` that is
        // reading or serving this open file's directory.
        let directory_lock = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.directory_lock())?;
        let _seeking = directory_lock.lock().await;
        self.adopt_rendered_procfs_position(guest, call.fd())
            .await?;
        let (fd_type, status_flags, procfs_position, resource, directory_stream, host_order) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.status_flags(),
                    detfd.procfs_position(),
                    detfd.resource(),
                    detfd.has_directory_stream(),
                    detfd.directory_in_host_order(),
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
        if directory_stream {
            // Positions in a sorted directory stream are entry indices (see
            // `DirectoryStream`), not host cookies. The kernel position only
            // follows the stream, for descriptors Detcore does not track. Like
            // tmpfs's `dcache_dir_lseek`, only SEEK_SET and SEEK_CUR are
            // accepted; SEEK_END, SEEK_DATA and SEEK_HOLE get `EINVAL`, where
            // ext4 and btrfs accept SEEK_END
            // (https://github.com/rrnewton/hermit/issues/3724).
            let current = guest.thread_state().with_detfd(call.fd(), |detfd| {
                detfd.with_directory_stream(|stream| stream.position())
            })??;
            let requested = i128::from(call.offset());
            let position = match call.whence() {
                Whence::SEEK_SET => requested,
                Whence::SEEK_CUR => i128::from(current) + requested,
                _ => return Err(Errno::EINVAL.into()),
            };
            let result = i64::try_from(position)
                .ok()
                .filter(|position| *position >= 0)
                .ok_or(Errno::EINVAL)?;
            let target = guest.thread_state().with_detfd(call.fd(), |detfd| {
                detfd.with_directory_stream(|stream| {
                    stream.seek(result as u64);
                    stream.kernel_target()
                })
            })??;
            self.move_directory_kernel_position(guest, call.fd() as RawFd, target)
                .await;
            return Ok(result);
        }
        let Some((current, snapshot_len)) = procfs_position else {
            // A directory without a stream yet: SEEK_END would return the
            // host directory's size, which st_size no longer reports and
            // which changes as anything outside the run adds or removes an
            // entry (https://github.com/rrnewton/hermit/issues/4372). Answer
            // as a stream does (above, tmpfs's `dcache_dir_lseek`), from the
            // descriptor's first use. An `O_PATH` descriptor goes on to the
            // kernel, which refuses its seek with EBADF whatever the whence.
            if guest.config().virtualize_metadata
                && status_flags & libc::O_PATH == 0
                && matches!(
                    call.whence(),
                    Whence::SEEK_END | Whence::SEEK_DATA | Whence::SEEK_HOLE
                )
            {
                let cached = guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| detfd.stat())?;
                if Self::is_seekable_directory(guest, call.fd(), cached).await {
                    return Err(Errno::EINVAL.into());
                }
            }
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
            let position = self.record_or_replay(guest, call).await?;
            if host_order && position == 0 {
                // Back at the start, the next `getdents` can read the whole
                // directory again and serve it as a sorted stream.
                guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| detfd.use_directory_stream())?;
            }
            return Ok(position);
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

        // A procfs file Detcore renders, whichever opened it: an `O_PATH`
        // descriptor has no position (EBADF), before the seek's arguments are
        // looked at; otherwise a sysctl file's `default_llseek` on a file of
        // size 0, whatever the snapshot holds, or a `seq_file`'s `seq_lseek`,
        // which seeks only from the start or the position.
        let files = self.rendered_files(guest);
        let rendered = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_renderable_path(files))?;
        if let Some(rendered) = rendered {
            if status_flags & libc::O_PATH != 0 {
                return Err(Errno::EBADF.into());
            }
            let seq_file = crate::procfs::rendered_path_is_seq_file(rendered);
            let requested = i128::from(call.offset());
            let new_offset = match call.whence() {
                Whence::SEEK_SET => requested,
                Whence::SEEK_CUR => current as i128 + requested,
                _ if seq_file => return Err(Errno::EINVAL.into()),
                Whence::SEEK_END => requested,
                // At or past the end, which is 0.
                Whence::SEEK_DATA | Whence::SEEK_HOLE if requested >= 0 => {
                    return Err(Errno::ENXIO.into());
                }
                Whence::SEEK_DATA | Whence::SEEK_HOLE => requested,
                _ => return Err(Errno::EINVAL.into()),
            };
            let new_offset = usize::try_from(new_offset).map_err(|_| Errno::EINVAL)?;
            let result = i64::try_from(new_offset).map_err(|_| Errno::EOVERFLOW)?;
            // seq_lseek to another positive position traverses the file,
            // which runs a per-process file's `show` (ESRCH once its task is
            // gone); a rewind or a no-op does not.
            if new_offset != 0 && new_offset != current {
                self.require_rendered_task(guest, call.fd()).await?;
            }
            guest
                .thread_state()
                .with_detfd(call.fd(), |detfd| detfd.seek_procfs(new_offset))?;
            self.sync_rendered_procfs_position(guest, call.fd()).await?;
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

        // A `seq_file` seeks only from the start or the position, before and
        // after the snapshot (`ProcfsFile::seeks_only_from_start_or_position`).
        let seq_file = guest.thread_state().with_detfd(call.fd(), |detfd| {
            detfd.procfs_seeks_only_from_start_or_position()
        })?;
        let requested = i128::from(call.offset());
        let new_offset = match call.whence() {
            Whence::SEEK_SET => requested,
            Whence::SEEK_CUR => current as i128 + requested,
            _ if seq_file => return Err(Errno::EINVAL.into()),
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
        self.sync_rendered_procfs_position(guest, call.fd()).await?;
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
                    detfd.stat().map(|stat| stat.raw_inode()),
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

        if call.len() > 0 {
            self.fifo_polls_if_a_guest_holds_its_peer(guest, call.fd(), FifoPeer::Reader)
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
                detfd.stat().map(|x| x.raw_inode()),
            )
        })?;
        // It doesn't matter much where the linearization point for this mtime bump falls:
        if guest.config().virtualize_metadata {
            let r =
                raw_ino.expect("Expect that when virtualize_metadata, DetFd's stat is populated!");
            touch_file(guest, r).await;
        }

        if let Some(resource) = resource {
            let container_output = crate::sigalrm_phase1::is_container_output(&resource);
            let mut request = guest.thread_state().mk_request(resource, Permission::W);
            if should_tag_host_timed_internal_pipe_io(
                guest.config().backend.internal_pipe_turns_are_host_timed,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(HOST_TIMED_INTERNAL_PIPE_IO_FYI);
            }
            resource_request(guest, request).await;
            // Signal phase 1, design closure 1: a write to inherited stdout or
            // stderr may sleep with no scheduler admission. Once the grant is
            // taken no SIGALRM commits until this call ends, so one already due
            // is the only one it can strand: recorded as a determinism loss.
            if guest.thread_state().sigalrm_handled && container_output {
                sigalrm_refuses(guest, SigalrmControl::UnadmittedStdioIo).await;
            }
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
            (detfd.resource(), detfd.stat().map(|stat| stat.raw_inode()))
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

        self.fifo_polls_if_a_guest_holds_its_peer(guest, call.fd(), FifoPeer::Reader)
            .await;
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
                detfd.stat().map(|x| x.raw_inode()),
            )
        })?;

        if let Some(resource) = resource {
            let mut request = guest.thread_state().mk_request(resource, Permission::W);
            if should_tag_host_timed_internal_pipe_io(
                guest.config().backend.internal_pipe_turns_are_host_timed,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(HOST_TIMED_INTERNAL_PIPE_IO_FYI);
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
    /// A vectored read (`readv`, or `preadv2` at the current offset) of a
    /// signalfd, whose
    /// SIGCHLD records go through the child-exit ledger as `read`'s do
    /// (`reconcile_signalfd_extents`).
    ///
    /// Linux imports the `count` iovecs at `address` before it writes any record,
    /// and the records can overwrite the array itself, so the geometry is
    /// captured here, before the read, and used for the reconciliation and
    /// for the output observation (`observed`) alike (Codex re-check #6 of
    /// https://github.com/rrnewton/hermit/pull/4031). A read whose every
    /// record is dropped reads again, with the array's bytes put back first.
    /// An array the tracer cannot read is a proven kernel failure (EINVAL, or
    /// EFAULT with some byte outside every mapping: the kernel copies the whole
    /// array), forwarded unchanged; a wholly mapped one it cannot read, or one
    /// whose mappings cannot be listed, is refused by name, so no record is
    /// returned unreconciled.
    async fn vectored_signalfd_read<G: Guest<Self>, C>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        call: C,
        address: usize,
        count: usize,
        observed: &mut Option<Vec<crate::io_buffers::BufferExtent>>,
    ) -> Result<i64, Error>
    where
        C: Into<Syscall> + Copy,
    {
        const IOVEC: usize = std::mem::size_of::<libc::iovec>();
        let fd = match call.into() {
            Syscall::Readv(call) => call.fd(),
            Syscall::Preadv2(call) => call.fd(),
            _ => -1,
        };
        // Linux passes the count on as an `unsigned int` (Codex re-check #7).
        let count = crate::io_buffers::linux_iovec_count(count as u64);
        if count > libc::UIO_MAXIOV as usize {
            return self
                .record_or_replay_preserving_tool_errors(guest, call)
                .await;
        }
        let mut raw = vec![0u8; count * IOVEC];
        if count > 0 {
            let read = AddrMut::<u8>::from_raw(address)
                .is_some_and(|at| guest.memory().read_exact(at, &mut raw).is_ok())
                || {
                    use std::os::unix::fs::FileExt;
                    std::fs::File::open(format!("/proc/{}/mem", guest.tid().as_raw()))
                        .and_then(|mem| mem.read_exact_at(&mut raw, address as u64))
                        .is_ok()
                };
            if !read {
                // A maps listing that cannot be read decides nothing: refuse.
                let maps = std::fs::read_to_string(format!("/proc/{}/maps", guest.tid().as_raw()));
                if let Ok(maps) = &maps
                    && !signalfd_range_is_mapped(maps, address, raw.len())
                {
                    return self
                        .record_or_replay_preserving_tool_errors(guest, call)
                        .await;
                }
                tracing::error!(
                    "[detcore, dtid {}] {} of a signalfd with its iovec array at {:#x} is not \
                     supported: the memory is mapped, but Hermit cannot read it to reconcile \
                     the SIGCHLD records (https://github.com/rrnewton/hermit/issues/3994)",
                    guest.thread_state().dettid,
                    sysno,
                    address,
                );
                return self
                    .refuse_unserviceable_operation(guest, sysno, Errno::ENOSYS)
                    .await;
            }
        }
        let extents: Vec<(usize, usize)> = raw
            .chunks(IOVEC)
            .map(|iovec| {
                let word = |at: usize| {
                    usize::from_ne_bytes(iovec[at..at + 8].try_into().expect("eight bytes"))
                };
                (word(0), word(8))
            })
            .collect();
        // A vectored read of a signalfd covering a signal with an entry behind
        // its list's front would need one record per kernel read, each into
        // its own sub-extents (`read_signalfd_per_record`), which Detcore does
        // not do for a vector; refused by name rather than return records out
        // of Linux's order (<https://github.com/rrnewton/hermit/issues/4262>).
        let total: usize = extents.iter().map(|(_, len)| *len).sum();
        if total >= 2 * crate::syscalls::signal::SIGNALFD_RECORD
            && self.signalfd_reads_per_record(guest, fd).await
        {
            tracing::error!(
                "[detcore, dtid {}] {} of a signalfd while a signal it covers has a POSIX timer's entry queued behind another is not supported: Linux returns them one by one in its dequeue order, which Detcore produces only for read (https://github.com/rrnewton/hermit/issues/4262)",
                guest.thread_state().dettid,
                sysno,
            );
            return self
                .refuse_unserviceable_operation(guest, sysno, Errno::ENOSYS)
                .await;
        }
        loop {
            let before =
                crate::syscalls::signal::capture_signalfd_buffer(guest, &extents, usize::MAX);
            let result = self
                .record_or_replay_preserving_tool_errors(guest, call)
                .await;
            match result {
                Ok(read) if read > 0 => {
                    let Some(kept) = self
                        .reconcile_signalfd_extents(guest, &extents, read as usize, &before)
                        .await
                    else {
                        break self.refuse_unrestorable_signalfd_record(guest, sysno).await;
                    };
                    if self
                        .settle_faulted_signalfd_record(guest, fd, &extents, Some(read as usize))
                        .await
                    {
                        break self.refuse_faulted_signalfd_discard(guest, sysno).await;
                    }
                    if kept > 0 {
                        *observed = Some(
                            crate::syscalls::signal::signalfd_extent_pieces(&extents, kept)
                                .into_iter()
                                .map(|(addr, len)| crate::io_buffers::BufferExtent {
                                    addr: addr as u64,
                                    len: len as u64,
                                })
                                .collect(),
                        );
                        break Ok(kept as i64);
                    }
                    if let Some(at) = AddrMut::<u8>::from_raw(address)
                        && guest.memory().write_exact(at, &raw).is_err()
                    {
                        break Err(Errno::EFAULT.into());
                    }
                }
                Err(Error::Errno(Errno::EFAULT)) => {
                    if self
                        .settle_faulted_signalfd_record(guest, fd, &extents, None)
                        .await
                    {
                        break self.refuse_faulted_signalfd_discard(guest, sysno).await;
                    }
                    break Err(Errno::EFAULT.into());
                }
                result => break result,
            }
        }
    }

    /// Linux's signalfd read dequeues each signal before it copies its record
    /// out, and stops at the first copy that faults, so that signal is lost:
    /// the read fails with EFAULT if it was the first record, or returns the
    /// records before it. The SIGCHLD such a record may have held is settled
    /// with the child-exit ledger as a faulted `rt_sigtimedwait`'s is
    /// (`WaitFaulted`; the Codex re-check of
    /// https://github.com/rrnewton/hermit/pull/4131 at 8692836e). `extents`
    /// are the read's buffers and `read` the bytes it returned, or `None` for
    /// an EFAULT.
    ///
    /// An EFAULT dequeued nothing when a buffer is not a user address at all
    /// (`access_ok`, checked before any dequeue). After a positive read, a
    /// record was lost only if the buffers had room for another and some
    /// byte of it is not writable; even then no signal may have been left to
    /// take, so the shared queue's pending bit decides that case.
    ///
    /// Returns whether a record may have been lost that way, and its copy may
    /// have been one Linux discarded when it was sent, which it would never
    /// have dequeued: the caller refuses the call by name
    /// (`refuse_faulted_signalfd_discard`), since the copy cannot be told
    /// (<https://github.com/rrnewton/hermit/issues/4293>).
    async fn settle_faulted_signalfd_record<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        extents: &[(usize, usize)],
        read: Option<usize>,
    ) -> bool {
        if !self.cfg.backend.reports_child_exit_publication || !guest.config().sequentialize_threads
        {
            return false;
        }
        match read {
            None => {
                let user_end = user_address_end();
                if extents.iter().any(|(address, len)| {
                    address.checked_add(*len).is_none_or(|end| end > user_end)
                }) {
                    return false;
                }
            }
            Some(read) => {
                let Some(next) = signalfd_next_record(extents, read) else {
                    return false;
                };
                let Ok(maps) =
                    std::fs::read_to_string(format!("/proc/{}/maps", guest.tid().as_raw()))
                else {
                    return false;
                };
                if next
                    .iter()
                    .all(|(address, len)| signalfd_range_is_writable(&maps, *address, *len))
                {
                    return false;
                }
            }
        }
        let mask = signalfd_sigmask(guest.pid().as_raw(), fd);
        let lost_a_discarded_copy = {
            use crate::scheduler::discarded_at_send::DiscardedSignalAnswer;
            use crate::scheduler::discarded_at_send::DiscardedSignalControl;
            use crate::scheduler::discarded_at_send::STANDARD_SIGNALS;
            matches!(
                crate::tool_global::discarded_signal(guest, DiscardedSignalControl::Bits).await,
                DiscardedSignalAnswer::Bits { thread, process }
                    if (thread | process) & mask.unwrap_or(!0) & STANDARD_SIGNALS != 0
            )
        };
        let sigchld = crate::syscalls::threads::kernel_sigset_bit(libc::SIGCHLD);
        if mask.is_some_and(|mask| mask & sigchld == 0) {
            return lost_a_discarded_copy;
        }
        let Ok(state) =
            crate::syscalls::threads::read_thread_wait_signal_state(guest.pid(), guest.tid())
        else {
            return lost_a_discarded_copy;
        };
        // A failed read of a descriptor for SIGCHLD alone took a SIGCHLD.
        let shared_pending = if read.is_none() && mask == Some(sigchld) {
            false
        } else {
            state.shared_pending & sigchld != 0
        };
        crate::tool_global::child_exit_sigchld(
            guest,
            crate::scheduler::child_exit_sigchld::ChildExitSigchldControl::WaitFaulted {
                private_pending: state.thread_pending & sigchld != 0,
                shared_pending,
            },
        )
        .await;
        lost_a_discarded_copy
    }

    /// Refuse by name a signalfd `sysno` whose copy-out faulted after a
    /// dequeue that may have taken a copy Linux discarded when it was sent
    /// (`settle_faulted_signalfd_record`): Linux would not have taken it, and
    /// which copy the lost record held cannot be told.
    async fn refuse_faulted_signalfd_discard<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
    ) -> Result<i64, Error> {
        tracing::error!(
            "[detcore, dtid {}] {} of a signalfd faulted copying out a record that may hold a \
             signal Linux discarded when it was sent, which Linux would never have dequeued; \
             Hermit cannot tell which copy was lost \
             (https://github.com/rrnewton/hermit/issues/4293)",
            guest.thread_state().dettid,
            sysno,
        );
        self.refuse_unserviceable_operation(guest, sysno, Errno::ENOSYS)
            .await
    }

    /// Refuse by name a signalfd `sysno` that read a record to drop (a copy
    /// Linux discarded at send, or a duplicate child-exit copy) whose bytes
    /// the capture before the read does not reach: more records than
    /// `SIGNALFD_CAPTURED` holds, which only real-time signals can make.
    /// Linux would leave those bytes as they were, and Hermit does not invent
    /// them (the Codex re-check of https://github.com/rrnewton/hermit/pull/4351,
    /// P2-2).
    async fn refuse_unrestorable_signalfd_record<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
    ) -> Result<i64, Error> {
        tracing::error!(
            "[detcore, dtid {}] {} of a signalfd read a record Hermit drops, beyond the bytes \
             captured before the read, so it cannot leave the caller's buffer as Linux would \
             (https://github.com/rrnewton/hermit/issues/4293)",
            guest.thread_state().dettid,
            sysno,
        );
        self.refuse_unserviceable_operation(guest, sysno, Errno::ENOSYS)
            .await
    }

    /// SYS_readv system call: the vectored form of `read`.
    ///
    /// Mirrors [`Self::handle_writev`] for the read direction. Detcore adds
    /// open-file resource ordering and, for physically nonblocking pipe/socket
    /// fds, the nonblocking scheduler integration. Random devices use the shared
    /// canonical cursor; other descriptors retain their recorded kernel operation.
    pub async fn handle_readv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readv,
    ) -> Result<i64, Error> {
        self.handle_readv_with_output(guest, call, &mut None).await
    }

    /// Import after the resource wait and retain that geometry through copyout.
    /// The shared-cursor closure is synchronous and takes no metadata locks.
    fn read_random_vectors<G: Guest<Self>>(
        &self,
        guest: &mut G,
        detfd: &DetFd,
        request: RandomVectoredRead,
        rng_output: &mut Option<Vec<crate::io_buffers::BufferExtent>>,
    ) -> Result<i64, Error> {
        require_random_device_read_access(detfd.status_flags())?;
        let iovecs = crate::iovecs::import_read_iovecs(
            &guest.memory(),
            request.address,
            request.count,
            || crate::iovecs::UserAddressLimit::from_query(guest.user_address_limit()),
        )?;
        let total = iovecs.iter().map(|iov| iov.len).sum();
        validate_random_vector_read(request.offset, total, request.flags)?;
        let written = if let Some(offset) = request.offset {
            self.fill_random_device_iovecs(guest, &iovecs, offset)?
        } else {
            detfd.with_random_device_stream(|offset| {
                self.fill_random_device_iovecs(guest, &iovecs, offset)
            })?
        };
        if written > 0 && self.cfg.detlog_io_buffers && crate::detlog_observed!() {
            *rng_output = Some(crate::io_buffers::rng_readv_extents(&iovecs, written)?);
        }
        Ok(written as i64)
    }

    pub(crate) async fn handle_readv_with_output<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readv,
        rng_output: &mut Option<Vec<crate::io_buffers::BufferExtent>>,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            self.require_timer_slack_access(guest, call.fd(), false)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), call.len())?;
            return self
                .readv_timer_slack(guest, call.fd(), iovecs, None, 0)
                .await;
        }
        let files = self.rendered_files(guest);
        if guest.thread_state().with_detfd(call.fd(), |detfd| {
            detfd.procfs_renderable_path(files).is_some()
        })? {
            let address = call.iov().map_or(0, |address| address.as_raw());
            return self
                .readv_rendered_procfs(guest, call.fd(), address, call.len(), None, 0)
                .await;
        }

        let is_procfs = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_position().is_some())?;
        if is_procfs {
            return Err(Errno::ENOSYS.into());
        }

        self.fifo_polls_if_a_guest_holds_its_peer(guest, call.fd(), FifoPeer::Writer)
            .await;
        let (fd_type, physically_nonblocking, logically_nonblocking, resource, detfd) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.physically_nonblocking(),
                    detfd.is_nonblocking(),
                    detfd.resource(),
                    detfd.clone(),
                )
            })?;

        if let Some(resource) = resource {
            let mut request = guest.thread_state().mk_request(resource, Permission::R);
            if should_tag_host_timed_internal_pipe_io(
                guest.config().backend.internal_pipe_turns_are_host_timed,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(HOST_TIMED_INTERNAL_PIPE_IO_FYI);
            }
            resource_request(guest, request).await;
        }

        let res = if fd_type == FdType::Rng {
            self.read_random_vectors(
                guest,
                &detfd,
                RandomVectoredRead {
                    address: call.iov().map_or(0, |addr| addr.as_raw()),
                    count: call.len(),
                    offset: None,
                    flags: 0,
                },
                rng_output,
            )
        } else if detfd.inherited_external_read().is_some() {
            // The same one-kernel-read rule as scalar input: preserve the
            // kernel's iovec/short-read/error semantics and announce blocking.
            self.record_or_replay_blocking(guest, call.into()).await
        } else if fd_type == FdType::Timerfd && crate::syscalls::timerfd::enabled(guest) {
            // As `read` of a timerfd on virtual time: publish its expirations,
            // retry a logically blocking read in turns, and return the
            // virtual count (`crate::syscalls::timerfd`).
            crate::syscalls::timerfd::publish_expired(guest).await;
            let result = self.execute_nonblockable_fd_syscall(guest, call).await;
            crate::syscalls::timerfd::finish_readv(guest, call.fd(), call.iov(), call.len(), result)
        } else if physically_nonblocking
            && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd)
        {
            self.execute_nonblockable_fd_syscall(guest, call).await
        } else if fd_type == FdType::Signalfd && self.cfg.backend.reports_child_exit_publication {
            // The SIGCHLD records a vectored read scatters go through the
            // child-exit ledger as `read`'s do (`reconcile_signalfd_extents`).
            self.vectored_signalfd_read(
                guest,
                Sysno::readv,
                call,
                call.iov().map_or(0, |iov| iov.as_raw()),
                call.len(),
                rng_output,
            )
            .await
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };

        // O2: a signalfd read (even one that faulted copying out a record
        // it had already dequeued) may have taken the process's ITIMER_REAL
        // copy (https://github.com/rrnewton/hermit/issues/4139).
        if fd_type == FdType::Signalfd {
            crate::tool_global::reconcile_alarm(guest).await;
        }
        resource_release_all(guest).await;
        res
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_preadv system call: the vectored form of `pread64`.
    ///
    /// RNG reads use the canonical stream at the explicit offset without
    /// advancing its shared cursor. Other files retain their kernel operation.
    pub async fn handle_preadv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Preadv,
    ) -> Result<i64, Error> {
        self.handle_preadv_with_output(guest, call, &mut None).await
    }

    pub(crate) async fn handle_preadv_with_output<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Preadv,
        rng_output: &mut Option<Vec<crate::io_buffers::BufferExtent>>,
    ) -> Result<i64, Error> {
        // Linux rejects negative offsets before descriptor lookup or import.
        let offset = vectored_offset(call.pos_l(), call.pos_h());
        if offset < 0 {
            return Err(Errno::EINVAL.into());
        }
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            self.require_timer_slack_access(guest, call.fd(), false)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), call.iov_len())?;
            return self
                .readv_timer_slack(guest, call.fd(), iovecs, Some(offset), 0)
                .await;
        }
        let files = self.rendered_files(guest);
        if guest.thread_state().with_detfd(call.fd(), |detfd| {
            detfd.procfs_renderable_path(files).is_some()
        })? {
            let address = call.iov().map_or(0, |address| address.as_raw());
            return self
                .readv_rendered_procfs(guest, call.fd(), address, call.iov_len(), Some(offset), 0)
                .await;
        }

        let is_procfs = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_position().is_some())?;
        if is_procfs {
            return Err(Errno::ENOSYS.into());
        }

        let detfd = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.clone())?;

        if let Some(resource) = detfd.resource() {
            let request = guest.thread_state().mk_request(resource, Permission::R);
            resource_request(guest, request).await;
        }

        let res = if detfd.ty() == FdType::Rng {
            self.read_random_vectors(
                guest,
                &detfd,
                RandomVectoredRead {
                    address: call.iov().map_or(0, |addr| addr.as_raw()),
                    count: call.iov_len(),
                    offset: Some(offset as u64),
                    flags: 0,
                },
                rng_output,
            )
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };
        resource_release_all(guest).await;
        res
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_preadv2 system call: positioned vectors, or the shared stream when
    /// offset is -1. RNG flag validation precedes output copying.
    pub async fn handle_preadv2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Preadv2,
    ) -> Result<i64, Error> {
        self.handle_preadv2_with_output(guest, call, &mut None)
            .await
    }

    pub(crate) async fn handle_preadv2_with_output<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Preadv2,
        rng_output: &mut Option<Vec<crate::io_buffers::BufferExtent>>,
    ) -> Result<i64, Error> {
        let offset = vectored_offset(call.pos_l(), call.pos_h());
        if offset < -1 {
            return Err(Errno::EINVAL.into());
        }
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
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
        let files = self.rendered_files(guest);
        if guest.thread_state().with_detfd(call.fd(), |detfd| {
            detfd.procfs_renderable_path(files).is_some()
        })? {
            let address = call.iov().map_or(0, |address| address.as_raw());
            let offset = (offset != -1).then_some(offset);
            return self
                .readv_rendered_procfs(
                    guest,
                    call.fd(),
                    address,
                    call.iov_len() as usize,
                    offset,
                    call.flags(),
                )
                .await;
        }

        let is_procfs = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_position().is_some())?;
        if is_procfs {
            return Err(Errno::ENOSYS.into());
        }

        let detfd = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.clone())?;

        if let Some(resource) = detfd.resource() {
            let request = guest.thread_state().mk_request(resource, Permission::R);
            resource_request(guest, request).await;
        }

        let res = if detfd.ty() == FdType::Rng {
            self.read_random_vectors(
                guest,
                &detfd,
                RandomVectoredRead {
                    address: call.iov().map_or(0, |addr| addr.as_raw()),
                    count: call.iov_len() as usize,
                    offset: (offset != -1).then_some(offset as u64),
                    flags: call.flags(),
                },
                rng_output,
            )
        } else if detfd.ty() == FdType::Signalfd
            && offset == -1
            && self.cfg.backend.reports_child_exit_publication
        {
            // At the current offset (-1) a signalfd reads as `readv` does,
            // and its records go through the child-exit ledger
            // (`reconcile_signalfd_extents`). At any other offset, as with
            // `preadv`, the kernel fails with ESPIPE before it imports the
            // array, so that call goes to it unchanged below.
            self.vectored_signalfd_read(
                guest,
                Sysno::preadv2,
                call,
                call.iov().map_or(0, |iov| iov.as_raw()),
                usize::try_from(call.iov_len()).unwrap_or(0),
                rng_output,
            )
            .await
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };
        // O2: a signalfd read (even one that faulted copying out a record
        // it had already dequeued) may have taken the process's ITIMER_REAL
        // copy (https://github.com/rrnewton/hermit/issues/4139).
        if detfd.ty() == FdType::Signalfd {
            crate::tool_global::reconcile_alarm(guest).await;
        }
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
            (detfd.resource(), detfd.stat().map(|stat| stat.raw_inode()))
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
            (detfd.resource(), detfd.stat().map(|stat| stat.raw_inode()))
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
        // MREMAP_DONTUNMAP (Linux 5.7): the old mapping stays in place.
        let keep_source = call.flags() & 4 != 0;
        let result = self.record_or_replay(guest, call).await?;
        let new_start =
            usize::try_from(result).expect("a successful mremap must return an address");
        guest
            .thread_state()
            .remap_memory(old_start, old_len, new_start, new_len, keep_source);
        Ok(result)
    }

    /// What Detcore learns of the file `stat` describes, which the guest
    /// named as `target`, as a procfs entry: its path within procfs when its
    /// device is a procfs mount in the guest's mount table (see
    /// [`procfs_inode`]). A failure to tell or to name a procfs entry records
    /// a determinism loss, as the entry's number then depends on whether the
    /// host rebuilt it.
    ///
    /// Never under record and replay, which reproduces the recorded host
    /// numbers instead: Replayer's descriptor is a placeholder, and its guest
    /// has no `/proc`. Nor when the guest's thread IDs do not name host tasks
    /// (see [`procfs_inode::guest_tids_name_host_tasks`]), as there is then no
    /// `/proc/<tid>` to read.
    ///
    /// Nor without sequentialized threads (see [`Self::name_procfs_entry`]).
    ///
    /// Naming a path injects an `openat` and a `close` into the guest (see
    /// [`Self::with_object_link`]), and Detcore trusts the host kernel to run
    /// them as Linux defines them. A
    /// seccomp filter the guest inherited from whatever started it (which
    /// `Config::inherited_seccomp` records) may refuse them with an error,
    /// such as `EPERM`, and Detcore then names nothing and records a
    /// determinism loss. A filter, or a user-space supervisor
    /// (`SECCOMP_RET_USER_NOTIF`), that returns a result without running the
    /// call stands in for the kernel, which Detcore does not support.
    ///
    /// The `O_PATH` descriptor that names a path is open in the guest for one
    /// turn. A call the scheduler runs in the background meanwhile, such as
    /// an `accept` on a descriptor Detcore does not track, can allocate a
    /// descriptor in the same table at a moment the host chooses, and so take
    /// another number than without the stat, as it can beside any other
    /// descriptor allocation (https://github.com/rrnewton/hermit/issues/3988).
    ///
    /// The guest's `errno` is what it was: under SaBRe, Detcore shares libc,
    /// and so `errno`, with the guest thread it handles (see
    /// [`procfs_inode::preserving_errno`]).
    async fn procfs_entry<G: Guest<Self>>(
        guest: &mut G,
        stat: &DetStat,
        target: StatTarget<'_>,
    ) -> ProcfsEntry {
        procfs_inode::preserving_errno(Self::name_procfs_entry(guest, stat, target)).await
    }

    /// Maps ownership needs the actual opened object even where scheduler
    /// IDs are virtual. Use the existing same-view task directory capability;
    /// never pass a virtual TID to the host-only procfs naming path.
    async fn maps_descriptor_entry<G: Guest<Self>>(
        &self,
        guest: &G,
        stat: &DetStat,
        fd: RawFd,
    ) -> ProcfsEntry {
        procfs_inode::preserving_errno(async {
            if guest.config().recordreplay_modes || !guest.config().sequentialize_threads {
                return ProcfsEntry::Unknown;
            }
            let Some(task) = self.guest_task_directory(guest) else {
                return ProcfsEntry::Unknown;
            };
            match procfs_inode::descriptor_procfs_path(&task, fd, stat.dev, stat.inode) {
                Ok(Some(within)) => ProcfsEntry::At(within),
                Ok(None) => ProcfsEntry::NotProcfs,
                Err(_) => ProcfsEntry::Unknown,
            }
        })
        .await
    }

    async fn maps_guest_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        actual: &Path,
    ) -> Option<crate::procfs::BoundTask> {
        let numbers = if Self::translates_procfs_numbers(guest) {
            let ids = crate::procfs::ids::named_ids(actual);
            Some(Self::guest_procfs_numbers(guest, &ids).await?)
        } else {
            None
        };
        let (pid, tid) = self.procfs_caller_ids(guest)?;
        Self::maps_target_with_numbers(actual, numbers.as_ref(), pid, tid)
    }

    /// Preserve an authenticated spelling when its complete numeric mapping
    /// is the identity. Missing target components cannot authorize a binding.
    fn maps_target_with_numbers(
        actual: &Path,
        numbers: Option<&BTreeMap<i32, i32>>,
        pid: i32,
        tid: i32,
    ) -> Option<crate::procfs::BoundTask> {
        let translated;
        let actual = if let Some(numbers) = numbers {
            if crate::procfs::ids::named_ids(actual)
                .iter()
                .any(|id| !numbers.contains_key(id))
            {
                return None;
            }
            translated = crate::procfs::ids::translate(actual, |id| numbers.get(&id).copied());
            translated.as_deref().unwrap_or(actual)
        } else {
            actual
        };
        crate::procfs::maps_task_at(actual, pid, tid)
    }

    /// [`Self::procfs_entry`], which restores the guest's `errno` afterwards.
    ///
    /// Without sequentialized threads, another guest thread can change what a
    /// descriptor, the working directory or a path leads to between any two
    /// of the steps that name an entry, and under SaBRe it shares the
    /// descriptor table Detcore reads the mount table through. So Detcore
    /// names nothing then, before any of those steps, and records a
    /// determinism loss for any file that may be on procfs (see
    /// [`procfs_inode::may_be_procfs`]), devpts files among them.
    async fn name_procfs_entry<G: Guest<Self>>(
        guest: &mut G,
        stat: &DetStat,
        target: StatTarget<'_>,
    ) -> ProcfsEntry {
        if guest.config().recordreplay_modes
            || !procfs_inode::guest_tids_name_host_tasks(
                &guest.config().backend,
                guest.thread_state().physical_tid,
            )
            || !procfs_inode::may_be_procfs(stat.dev, stat.blksize)
        {
            return ProcfsEntry::NotProcfs;
        }
        if !guest.config().sequentialize_threads {
            Self::record_procfs_loss(
                guest,
                "the guest's threads are not sequentialized, so Detcore does not name procfs files"
                    .into(),
            )
            .await;
            return ProcfsEntry::Unknown;
        }
        let tid = guest.tid().as_raw();
        let mounts = match procfs_inode::procfs_mounts(tid, stat.dev, false) {
            Ok(Some(mounts)) => mounts,
            // No procfs mount in the guest's mount table is on the device, but
            // a procfs file can be, opened outside the guest's mounts: one
            // inherited from Hermit's own `/proc`, or received through
            // SCM_RIGHTS. It has no path within the guest's procfs, so it goes
            // unnamed. Its filesystem type tells.
            Ok(None) => {
                let on_procfs = Self::with_object_link(guest, target, |link| {
                    procfs_inode::is_on_procfs(link, stat.dev, stat.inode)
                })
                .await;
                return match on_procfs {
                    Ok(false) => ProcfsEntry::NotProcfs,
                    Ok(true) => {
                        Self::record_procfs_loss(
                            guest,
                            "a procfs file outside the guest's procfs mounts".into(),
                        )
                        .await;
                        ProcfsEntry::Unnamed
                    }
                    Err(reason) => {
                        Self::record_procfs_loss(guest, reason).await;
                        ProcfsEntry::Unknown
                    }
                };
            }
            Err(error) => {
                Self::record_procfs_loss(
                    guest,
                    format!(
                        "cannot read the guest's mount table: {}",
                        crate::util::describe_io_error(&error)
                    ),
                )
                .await;
                return ProcfsEntry::Unknown;
            }
        };
        let (canonical, mount_id) = match Self::procfs_canonical_path(guest, stat, target).await {
            Ok(found) => found,
            Err(reason) => {
                Self::record_procfs_loss(guest, reason).await;
                return ProcfsEntry::Unnamed;
            }
        };
        let reread = || procfs_inode::procfs_mounts(tid, stat.dev, true);
        match procfs_inode::path_within(
            &mounts,
            mount_id,
            &canonical,
            reread,
            procfs_inode::mount_root,
        ) {
            Ok(within) => ProcfsEntry::At(within),
            // The guest reached the file through a mount that its mount
            // table, read again, does not list at a mount point where Detcore
            // finds the mount: one outside the guest's root, or one whose
            // mount point reads in another frame than the file's path.
            Err(reason) => {
                Self::record_procfs_loss(guest, reason).await;
                ProcfsEntry::Unnamed
            }
        }
    }

    /// The canonical path of the procfs file `stat` describes, which the
    /// guest named as `target`, from a magic link to it (see
    /// [`Self::with_object_link`]), if that is still the file `stat`
    /// describes, and the ID of the mount it was reached through. An entry
    /// the host rebuilt between the guest's stat and an `O_PATH` open, a
    /// window of one injected call, is not, and goes unnamed.
    async fn procfs_canonical_path<G: Guest<Self>>(
        guest: &mut G,
        stat: &DetStat,
        target: StatTarget<'_>,
    ) -> Result<(PathBuf, u64), String> {
        Self::with_object_link(guest, target, |link| {
            procfs_inode::inspect_link(link, stat.dev, stat.inode)
        })
        .await
    }

    /// Run `inspect` on a magic link to the file the guest named as
    /// `target`: that of the descriptor or working directory in the guest
    /// thread's `/proc/<tid>`. For a path, Detcore opens an `O_PATH`
    /// descriptor in the guest with the guest's own path, directory and
    /// flags, which resolves `self` and `thread-self` to the guest, and
    /// closes it again (see [`Self::open_object_descriptor`] and
    /// [`Self::close_object_descriptor`]). An error is the reason Detcore
    /// learned nothing, or that the guest's descriptors may have changed.
    ///
    /// It runs only with sequentialized threads (see
    /// [`Self::name_procfs_entry`]). Only then do the open, the look at the
    /// link and the close run in one turn, and does the guest's descriptor or
    /// working directory lead to one file throughout; otherwise another guest
    /// thread can close the descriptor Detcore opened and take its number for
    /// one of its own in between, which Detcore would then read, and close.
    async fn with_object_link<G: Guest<Self>, R>(
        guest: &mut G,
        target: StatTarget<'_>,
        inspect: impl FnOnce(&Path) -> Result<R, String>,
    ) -> Result<R, String> {
        let tid = guest.tid().as_raw();
        match target {
            StatTarget::Descriptor(fd) => inspect(Path::new(&format!("/proc/{tid}/fd/{fd}"))),
            StatTarget::Cwd => inspect(Path::new(&format!("/proc/{tid}/cwd"))),
            StatTarget::Overwritten => Err("the call's result overwrote the guest's path".into()),
            StatTarget::Path {
                dirfd,
                path,
                follow,
            } => {
                let mut flags = OFlag::O_PATH | OFlag::O_CLOEXEC;
                if !follow {
                    flags |= OFlag::O_NOFOLLOW;
                }
                let open = syscalls::Openat::new()
                    .with_dirfd(dirfd)
                    .with_path(path)
                    .with_flags(flags);
                let fd = Self::open_object_descriptor(guest, open).await?;
                let found = inspect(Path::new(&format!("/proc/{tid}/fd/{fd}")));
                Self::close_object_descriptor(guest, fd).await?;
                found
            }
        }
    }

    /// Open the `O_PATH` descriptor `open` describes in the guest. An open
    /// that opened no descriptor and reported a restart or `EINTR` (see
    /// [`procfs_inode::read_open`]) is tried again, but only
    /// `OBJECT_DESCRIPTOR_ATTEMPTS` times, as a seccomp filter the guest
    /// inherited can return a restart every time without running the call.
    async fn open_object_descriptor<G: Guest<Self>>(
        guest: &mut G,
        open: syscalls::Openat,
    ) -> Result<RawFd, String> {
        let mut last = Errno::EINTR.to_string();
        for _ in 0..OBJECT_DESCRIPTOR_ATTEMPTS {
            match procfs_inode::read_open(guest.inject(open).await) {
                procfs_inode::Injected::Ran(fd) => return Ok(fd),
                procfs_inode::Injected::NotRun(reported) => last = reported,
                procfs_inode::Injected::Failed(reason) => return Err(reason),
            }
        }
        Err(format!(
            "cannot open the guest's path again: {last} in each of {OBJECT_DESCRIPTOR_ATTEMPTS} attempts"
        ))
    }

    /// Close the `O_PATH` descriptor `fd` that Detcore opened in the guest. A
    /// close that did not run (see [`procfs_inode::read_close`]) is tried
    /// again, but only `OBJECT_DESCRIPTOR_ATTEMPTS` times. One that failed
    /// leaves the descriptor open, where it changes the numbers the guest's
    /// later descriptors take, so it is an error.
    async fn close_object_descriptor<G: Guest<Self>>(
        guest: &mut G,
        fd: RawFd,
    ) -> Result<(), String> {
        let close = syscalls::Close::new().with_fd(fd);
        for _ in 0..OBJECT_DESCRIPTOR_ATTEMPTS {
            match procfs_inode::read_close(guest.inject(close).await) {
                procfs_inode::Injected::Ran(()) => return Ok(()),
                procfs_inode::Injected::NotRun(_) => {}
                procfs_inode::Injected::Failed(reason) => return Err(reason),
            }
        }
        Err(format!(
            "close of the guest's O_PATH descriptor did not run in {OBJECT_DESCRIPTOR_ATTEMPTS} attempts"
        ))
    }

    /// Records the determinism loss of a procfs entry Detcore could not name,
    /// once per process, through the global tool, whose process is the one
    /// whose losses verification reads under every backend.
    async fn record_procfs_loss<G: Guest<Self>>(guest: &mut G, reason: String) {
        static RECORDED: AtomicBool = AtomicBool::new(false);
        if !RECORDED.swap(true, Ordering::Relaxed) {
            report_determinism_loss(guest, format!("procfs inode numbers: {reason}")).await;
        }
    }

    // Determinize stat by doing:
    //   - using virtual inode instead of real inodes. The virtual inodes
    //     increase monolitically and won't be re-used (like ext4)
    //   - use logical modtime which could be used by program like GNU make
    //     to determine file changes. A file first seen with a canonical host
    //     mtime (`CANONICAL_FILE_MTIME_SECONDS`, e.g. the Nix store's 1) keeps
    //     it; any other first-seen file reports the epoch.
    //   - atime, ctime and btime always report the epoch: the kernel sets ctime
    //     and btime itself, so even for a Nix store file they are real
    //     timestamps, and Hermit keeps no per-file atime.
    async fn determinize_stat<G, S>(
        &self,
        guest: &mut G,
        stat: S,
        inode_override: Option<DetInode>,
        procfs: ProcfsEntry,
    ) -> Result<DetStat, Error>
    where
        G: Guest<Self>,
        S: Into<DetStat>,
    {
        let cfg = guest.config().clone();

        let mut stat: DetStat = stat.into();
        // Detcore's placeholder, however the guest reached it: the rendered
        // file it stands in for.
        if inode_override.is_none()
            && !cfg.recordreplay_modes
            && let Some(path) = rendered_procfs::placeholder_identity(stat.dev, stat.inode)
        {
            return self.rendered_metadata(guest, path).await;
        }
        let (d_ino, global_mtime) = match inode_override {
            // The container's stdio streams have fixed inodes and always
            // report the epoch: whatever backs them on the host (a pipe, a
            // terminal, a redirected file) is not part of the guest's view.
            Some(inode) => {
                let nanos = cfg
                    .epoch
                    .timestamp_nanos_opt()
                    .expect("epoch cannot be represented in nanoseconds")
                    as u64;
                (inode, LogicalTime::from_nanos(nanos))
            }
            None => {
                // statx fills stx_mtime only when it reports STATX_MTIME.
                let observed = if stat.mask.contains(StatxMask::STATX_MTIME) {
                    ObservedMtime::from_host_mtime(stat.mtime.tv_sec, stat.mtime.tv_nsec)
                } else {
                    ObservedMtime::Unobserved
                };
                // A file with a link cannot be one a retired mapping describes:
                // Linux refuses to link an unlinked file again (only an
                // `O_TMPFILE` file, which is never retired, may get a name).
                // So a nonzero link count is a name sighting however the file
                // was reached, by path, descriptor or working directory, and a
                // zero count (`fstat` after `unlink`, `/proc/self/fd/N`) is not.
                let sighting = if stat.mask.contains(StatxMask::STATX_NLINK) && stat.nlink > 0 {
                    InodeSighting::Name
                } else {
                    InodeSighting::Descriptor
                };
                // A procfs root counts the host's processes as its links.
                // Report 1, which file systems that do not count a
                // directory's links report, so that a walker such as `find`
                // infers nothing from it.
                if procfs.is_procfs()
                    && stat.inode == procfs_inode::PROC_ROOT_INO
                    && stat.mode & libc::S_IFMT == libc::S_IFDIR
                    && stat.mask.contains(StatxMask::STATX_NLINK)
                {
                    stat.nlink = 1;
                }
                match rendered_entry(&procfs, &stat, &cfg, self.rendered_files(guest)) {
                    // A file Detcore renders, reached through a spelling the
                    // kernel resolved (a symbolic link): the number its path
                    // names, and the epoch.
                    Some(path) => {
                        let nanos = cfg
                            .epoch
                            .timestamp_nanos_opt()
                            .expect("epoch cannot be represented in nanoseconds")
                            as u64;
                        (
                            DetInode::mint(procfs_inode::rendered_inode(path)),
                            LogicalTime::from_nanos(nanos),
                        )
                    }
                    None => {
                        determinize_inode_observing_mtime(
                            guest,
                            stat.raw_inode(),
                            procfs.name(stat.dev),
                            observed,
                            sighting,
                        )
                        .await
                    }
                }
            }
        };
        stat.inode = d_ino.as_raw(); // Reveal only the deterministic inode.

        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping.
        // The raw st_dev leaks the kernel's host-wide anonymous block-device
        // number for procfs/sysfs/tmpfs mounts, which drifts between runs (and
        // between the two runs of `--verify`). Reveal only a deterministic
        // device id.
        let host_device = stat.dev;
        stat.dev = determinize_device(guest, stat.dev).await;

        // A directory's size is whatever its file system makes of its
        // entries: btrfs sums the names' lengths, tmpfs counts entries, ext4
        // rounds up to its blocks. So it changes whenever any process adds or
        // removes an entry, and differs between file systems. Builds stat
        // every ancestor of their working directory (kbuild, configure,
        // cargo, CMake's look at $HOME), so the host's value broke
        // `--verify --verify-strict` of ordinary builds on a shared machine
        // and reproducibility across hosts
        // (https://github.com/rrnewton/hermit/issues/4162). Report one
        // 4096-byte block, as ext4 does for a small directory, whatever the
        // directory holds; btrfs reports an empty directory as 0, so even
        // an empty one is set. A procfs directory keeps the host's value
        // (0, or a descriptor count for /proc/<pid>/fd), which Detcore
        // already virtualizes there. Only a proved procfs directory keeps
        // it: one whose entry Detcore names (`ProcfsEntry`, on the backends
        // whose guest threads are host tasks) or KVM's synthetic procfs
        // (`procfs_inode::is_backend_procfs_device`). Its stat fields alone
        // prove nothing: an anonymous device with 1024-byte blocks is also
        // devpts, autofs or a FUSE mount reporting those blocks (Codex
        // re-check of https://github.com/rrnewton/hermit/pull/4369). Under
        // DBT, where Detcore names no procfs entries, such a directory keeps
        // the host's value unproved
        // (`procfs_inode::unprovable_procfs_signature`). sysfs and cgroup
        // directories, which Linux also reports as 0, report 4096.
        if stat.mode & libc::S_IFMT == libc::S_IFDIR
            && stat.mask.contains(StatxMask::STATX_SIZE)
            && !procfs.is_procfs()
            && !procfs_inode::is_backend_procfs_device(
                &guest.config().backend,
                host_device,
                stat.blksize,
            )
            && !procfs_inode::unprovable_procfs_signature(
                &guest.config().backend,
                guest.thread_state().physical_tid,
                host_device,
                stat.blksize,
            )
        {
            stat.size = 4096;
            if stat.mask.contains(StatxMask::STATX_BLOCKS) {
                stat.blocks = 8;
            }
        }

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
            // The target is found before the call, which can overwrite its path.
            // A procfs file Detcore renders is answered from the path as named,
            // as Linux reads the path before it writes the result, once the
            // call's own arguments passed.
            let target = stat_family_target(&guest.memory(), &call);
            if !stat_family_arguments_valid(&guest.memory(), &call) {
                if !matches!(self.rendered_procfs_stat(guest, target).await, Ok(None)) {
                    return Err(Errno::EINVAL.into());
                }
            } else if let Some(stat) = self.rendered_procfs_stat(guest, target).await? {
                let statptr = call.stat().ok_or(Errno::EFAULT)?;
                guest.memory().write_value(statptr.0, &stat.into())?;
                return Ok(0);
            }
            let target = match call.stat() {
                Some(result) => target.unless_overwritten(
                    &guest.memory(),
                    result.0.as_raw(),
                    std::mem::size_of::<libc::stat>(),
                ),
                None => target,
            };
            Self::inject_translated(guest, Syscall::from(call)).await?;
            let statptr = call.stat().ok_or(Errno::EFAULT)?;
            let inode_override = target
                .descriptor()
                .and_then(|fd| stdio_inode_override(guest, fd));
            let mut memory = guest.memory();
            let stat: libc::stat = memory.read_value(statptr.0)?;
            // A placeholder another process made, or an earlier image of this
            // one, becomes known by its identity (`determinize_stat`).
            if inode_override.is_none() {
                let _ = self
                    .recognize_placeholder(guest, &DetStat::from(&stat), target)
                    .await;
            }
            let procfs = match inode_override {
                Some(_) => ProcfsEntry::NotProcfs,
                None => Self::procfs_entry(guest, &DetStat::from(&stat), target).await,
            };
            let stat = self
                .determinize_stat(guest, stat, inode_override, procfs)
                .await?;
            memory.write_value(statptr.0, &stat.into())?;
            Ok(0)
        } else {
            // A procfs file Detcore renders has no kernel object to ask.
            let target = stat_family_target(&guest.memory(), &call);
            if !stat_family_arguments_valid(&guest.memory(), &call) {
                if !matches!(self.rendered_procfs_stat(guest, target).await, Ok(None)) {
                    return Err(Errno::EINVAL.into());
                }
            } else if let Some(stat) = self.rendered_procfs_stat(guest, target).await? {
                let statptr = call.stat().ok_or(Errno::EFAULT)?;
                guest.memory().write_value(statptr.0, &stat.into())?;
                return Ok(0);
            }
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// statx system call
    pub async fn handle_statx<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Statx,
    ) -> Result<i64, Error> {
        // Whether `statx` accepts its arguments, which it checks before it
        // looks at any file.
        let renderable = statx_arguments_valid(
            call.flags().bits(),
            call.mask().bits(),
            names_no_path(&guest.memory(), call.path(), call.flags()),
        );
        if guest.config().virtualize_metadata {
            // NB: let kernel handle error codes, it's not easy to do so without kernel
            // because there're many corner cases. i.e.: even access filepath from tracer
            // may cause tracer to hang under certain fuse filesystem (squashfs_ll).
            // The target is found before the call, which can overwrite its path.
            // A procfs file Detcore renders is answered from the path as named,
            // as Linux reads the path before it writes the result, once the
            // call's own arguments passed (which `statx` checks before it
            // looks up any file).
            let target = StatTarget::at(&guest.memory(), call.dirfd(), call.path(), call.flags());
            if !renderable {
                if !matches!(self.rendered_procfs_stat(guest, target).await, Ok(None)) {
                    return Err(Errno::EINVAL.into());
                }
            } else if let Some(stat) = self.rendered_procfs_stat(guest, target).await? {
                return self.write_rendered_statx(guest, call, stat).await;
            }
            let target = match call.statx() {
                Some(result) => target.unless_overwritten(
                    &guest.memory(),
                    result.0.as_raw(),
                    std::mem::size_of::<reverie::syscalls::linux_abi::Statx>(),
                ),
                None => target,
            };
            // A call that asks for the unique mount ID asks the kernel for the
            // classic one: the run's mount-ID pool numbers classic IDs, the
            // ones mountinfo and fdinfo show, and the guest's unique ID is
            // derived from that number (`fill_statx_mount_id`). Linux fills
            // both mount-ID fields in `vfs_statx_path` (v6.12
            // fs/stat.c:248-259) after the filesystem's getattr, which sees
            // `STATX_MNT_ID` in place of `STATX_MNT_ID_UNIQUE`.
            let unique = statx_requested_mask(call) & STATX_MNT_ID_UNIQUE != 0;
            Self::inject_translated(guest, Syscall::from(statx_asking_classic_mount_id(call)))
                .await?;
            let statptr = call.statx().ok_or(Errno::EFAULT)?;
            let inode_override = target
                .descriptor()
                .and_then(|fd| stdio_inode_override(guest, fd));
            let mut memory = guest.memory();
            let stat: reverie::syscalls::linux_abi::Statx = memory.read_value(statptr.0)?;
            // A placeholder another process made, or an earlier image of this
            // one, becomes known by its identity (`determinize_stat`).
            let placeholder = match inode_override {
                None => {
                    self.recognize_placeholder(guest, &DetStat::from(stat), target)
                        .await
                }
                Some(_) => None,
            };
            let procfs = match inode_override {
                Some(_) => ProcfsEntry::NotProcfs,
                None => Self::procfs_entry(guest, &DetStat::from(stat), target).await,
            };
            // A rendered file reached through any spelling (a symbolic link to
            // its descriptor link) is on the guest's procfs mount, not on the
            // placeholder's memfd's.
            let raw_mount_id = match placeholder {
                Some(_) => rendered_procfs::guest_procfs_mount_id().ok(),
                None => (stat.stx_mask & reverie::syscalls::linux_abi::STATX_MNT_ID != 0)
                    .then_some(stat.stx_mnt_id),
            };
            let stat = self
                .determinize_stat(guest, stat, inode_override, procfs)
                .await?;
            let mut statx: reverie::syscalls::linux_abi::Statx = stat.into();
            self.fill_statx_mount_id(guest, &mut statx, raw_mount_id, unique)
                .await;
            memory.write_value(statptr.0, &statx)?;
            Ok(0)
        } else {
            // A procfs file Detcore renders has no kernel object to ask.
            let target = StatTarget::at(&guest.memory(), call.dirfd(), call.path(), call.flags());
            // Arguments `statx` refuses before it looks up any file, here a
            // file Detcore renders.
            if !renderable {
                if !matches!(self.rendered_procfs_stat(guest, target).await, Ok(None)) {
                    return Err(Errno::EINVAL.into());
                }
            } else if let Some(stat) = self.rendered_procfs_stat(guest, target).await? {
                return self.write_rendered_statx(guest, call, stat).await;
            }
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// Whether a guest's F_SETFL or FIONBIO on this descriptor changes only its
    /// logical (guest-visible) O_NONBLOCK, leaving it physically nonblocking for
    /// the scheduler's nonblockize-and-retry.
    ///
    /// Outside record/replay that holds for every scheduler-managed type. In
    /// record/replay it holds only for a container-internal channel that is
    /// already physically nonblocking: a pipe, which is how `handle_pipe2`
    /// leaves every guest-created pipe in both phases, or a socketpair endpoint,
    /// which is how `handle_socketpair` leaves both ends. Forwarding the guest's
    /// clear would make the recording's next read on that channel a blocking
    /// read on an fd that `syscall_targets_internal_fd` calls internal, which
    /// deadlocks the sequentialized scheduler while the writer waits for its
    /// turn. Both phases take the same branch, because `fd_type`,
    /// `socketpair_endpoint` and `physically_nonblocking` are Detcore
    /// bookkeeping that record and replay evolve identically: an F_SETFL is
    /// still recorded and replayed, with the kernel given `flags | O_NONBLOCK`
    /// in record and the recorded result returned in replay; a FIONBIO returns
    /// before `record_or_replay`, so neither phase records or applies it. Other
    /// sockets and eventfds stay external in record/replay. A pipe whose
    /// physical O_NONBLOCK came from the guest rather than from Detcore is also
    /// kept nonblocking; in record/replay the only such pipes are `pipe2` pipes,
    /// which Detcore has already made nonblocking.
    fn keeps_physically_nonblocking(
        &self,
        fd_type: FdType,
        socketpair_endpoint: bool,
        physically_nonblocking: bool,
        inherited_external_read: bool,
    ) -> bool {
        self.cfg.use_nonblocking_sockets()
            && !inherited_external_read
            && match fd_type {
                FdType::Pipe => !self.cfg.recordreplay_modes || physically_nonblocking,
                FdType::Socket if socketpair_endpoint => {
                    !self.cfg.recordreplay_modes || physically_nonblocking
                }
                FdType::Socket | FdType::Eventfd => !self.cfg.recordreplay_modes,
                _ => false,
            }
    }

    /// fcntl system call
    pub async fn handle_fcntl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Fcntl,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        // Before anything else, so a refused call has no other effect.
        let (call, _owner_copy) = self.screen_fcntl_owner(guest, call).await?;
        // The owner of an asynchronous-I/O descriptor may be any process or
        // process group, and Linux signals it when data arrives, at a moment
        // set by host timing. Record the signals in this turn, before the call
        // reaches the kernel, for every process of the container, and never
        // forget them (`Scheduler::record_host_timed_signals`).
        let signals = fcntl_host_timed_signals(call.cmd());
        if signals & kernel_sigset_bit(libc::SIGALRM) != 0 {
            refuse_sigalrm(guest, SigalrmControl::ArmProducer).await?;
        }
        // A lease makes another process's open of the file wait for the
        // lease break, interruptibly, with no scheduler admission.
        if matches!(call.cmd(), F_SETLEASE(lease) if lease != libc::F_UNLCK) {
            refuse_sigalrm(guest, SigalrmControl::ArmProducer).await?;
        }
        if signals != 0 && guest.config().backend.host_timed_signals_are_physical {
            record_host_timed_signals(guest, HostTimedSignalScope::Container, signals).await;
        }
        let o_cloexec = match call.cmd() {
            F_DUPFD_CLOEXEC(_) => OFlag::O_CLOEXEC,
            _ => OFlag::empty(),
        };
        match call.cmd() {
            F_GETFL => {
                let physical_flags = self.record_or_replay(guest, call).await?;
                let (logical_nonblocking, rendered_nofollow) =
                    guest.thread_state().with_detfd(fd, |detfd| {
                        (
                            detfd.is_nonblocking(),
                            detfd.procfs_rendered_path().is_some()
                                && detfd.status_flags() & libc::O_NOFOLLOW != 0,
                        )
                    })?;
                // A rendered procfs file's placeholder was reopened without
                // the guest's `O_NOFOLLOW`, which Linux keeps.
                let physical_flags = if rendered_nofollow {
                    physical_flags | i64::from(libc::O_NOFOLLOW)
                } else {
                    physical_flags
                };
                let nonblocking = i64::from(OFlag::O_NONBLOCK.bits());
                if logical_nonblocking {
                    Ok(physical_flags | nonblocking)
                } else {
                    Ok(physical_flags & !nonblocking)
                }
            }
            F_SETFL(flags) => {
                let (fd_type, socketpair_endpoint, physically_nonblocking, inherited_external_read) =
                    guest.thread_state().with_detfd(fd, |detfd| {
                        (
                            detfd.ty(),
                            detfd.is_socketpair_endpoint(),
                            detfd.physically_nonblocking(),
                            detfd.inherited_external_read().is_some(),
                        )
                    })?;
                let force_nonblocking = self.keeps_physically_nonblocking(
                    fd_type,
                    socketpair_endpoint,
                    physically_nonblocking,
                    inherited_external_read,
                );
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
                let lock_tracked = self.lock_tracked_descriptions(guest, newfd..=newfd);
                let replaced = guest.thread_state_mut().dup_fd(fd, newfd, o_cloexec)?;
                if let Some(open_file_id) = replaced {
                    self.release_port_for_open_file(guest, open_file_id).await;
                }
                self.report_lock_references(guest, lock_tracked).await;
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
            // input, and `tests/c/pipe_capacity.c` locks
            // it as a guest-visible contract: that fixture shrinks to one page
            // and requires the value to round-trip. Clamping every
            // `F_SETPIPE_SZ` to the pinned capacity would break that contract
            // while fixing nothing that is actually nondeterministic.
            // A host-backed pipe's capacity is host state
            // (`reported_pipe_capacity`); the guest reads it no larger than
            // the ceiling it may raise a pipe to.
            F_GETPIPE_SZ => Ok(reported_pipe_capacity(
                self.record_or_replay(guest, call).await?,
            )),
            F_SETPIPE_SZ(requested) if pipe_capacity_request_exceeds_ceiling(requested) => {
                trace!(
                    "[detcore] refusing F_SETPIPE_SZ({}) above the deterministic pipe ceiling {}",
                    requested, DETERMINISTIC_PIPE_CAPACITY_BYTES
                );
                Err(Errno::EPERM.into())
            }
            // An OFD lock belongs to its open file description, and Linux
            // shows it in `/proc/locks` from every PID namespace, the host's
            // included, as owner -1. After each call, the description's own
            // fdinfo says which locks it holds (`procfs::locks`); the lock
            // itself is the kernel's, as for POSIX locks.
            F_OFD_SETLK(_) | F_OFD_SETLKW(_) => {
                let result = self.record_or_replay(guest, call).await?;
                self.refresh_description_locks(guest, fd, LockCall::Fcntl)
                    .await?;
                Ok(result)
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

    /// Record which OFD and flock locks the open file description `fd` names
    /// holds now, after a call through it that may have changed them
    /// (`procfs::locks`), from the `lock:` lines of its fdinfo, which name
    /// exactly that description's locks with the device `/proc/locks`
    /// prints. A descriptor Detcore keeps no model of is left out.
    ///
    /// The fdinfo is read only through an anchor proved to be procfs, by a
    /// sequence of calls through the recorder ([`Self::read_own_fdinfo`]):
    /// the `/proc/locks` rows a replay filters are the recording's, and
    /// replay takes no real lock, so the locks they are matched against must
    /// be the recording's too. A recording therefore holds, after each such
    /// call, the anchor's `openat`, `fstatfs` and `fstat`, the entry's
    /// `openat2`, `fstatfs` and `fstat`, the reads and two `close`s
    /// (hermit-cli's `RECORD_VERSION` names the change).
    ///
    /// The guest's `call` is never affected. A read that fails for a cause
    /// the guest's own filesystem view or the backend decides (no `/proc`,
    /// not procfs, an entry outside it or across a mount, a backend's
    /// refusal) makes the description untrackable: it contributes no lock. A
    /// read that fails for ambient or resource state (a full descriptor table
    /// or file table, no memory, an interruption, an I/O error, a kernel
    /// without `openat2`, a permission refusal) leaves its locks unknown, and
    /// a later `/proc/locks` read
    /// refuses while a guest table references the description
    /// (`procfs::locks::LockLedger::unknown`).
    ///
    /// A lock or unlock through a descriptor whose description the ledger
    /// may know under another identity (one Detcore keeps no model of, such
    /// as a descriptor received over `SCM_RIGHTS` on ptrace, or one an
    /// in-guest backend registered on reception) can leave that other
    /// identity's entry stale, so no lock on that file's inode number counts
    /// from then on (`procfs::locks::LockLedger::alias_inode`): hidden, never
    /// a host's row admitted. If that descriptor's fdinfo cannot be read for
    /// ambient or resource state, every later `/proc/locks` read refuses
    /// instead ([`Self::note_alias`]).
    pub(crate) async fn refresh_description_locks<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        call: LockCall,
    ) -> Result<(), Error> {
        let model = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_lock_tracked();
            if call == LockCall::Recvmsg {
                detfd.set_received_for_locks();
            }
            if call == LockCall::Fcntl {
                detfd.set_may_hold_ofd_lock();
            }
            (detfd.open_file_id(), detfd.is_received_for_locks())
        });
        let Ok((open_file, received)) = model else {
            if call.changes_locks() {
                let fdinfo = self.read_own_fdinfo(guest, fd, call.name()).await?;
                Self::note_alias(guest, &fdinfo).await;
            }
            return Ok(());
        };
        let fdinfo = self.read_own_fdinfo(guest, fd, call.name()).await?;
        if received && call.changes_locks() {
            Self::note_alias(guest, &fdinfo).await;
        }
        let held = matches!(&fdinfo, FdinfoRead::Contents(_));
        let locks = match fdinfo {
            FdinfoRead::Contents(contents) => {
                Ok(crate::procfs::locks::held_locks_in_fdinfo(&contents))
            }
            FdinfoRead::Unknown(reason) => Err(reason),
            // Hidden, never admitted: the description contributes no lock
            // until a later read through a proved procfs.
            FdinfoRead::Untrackable(reason) => {
                info!("[detcore] /proc/locks: {reason}; its locks are not shown");
                Ok(Vec::new())
            }
        };
        if !held
            || locks
                .as_ref()
                .is_ok_and(|locks| locks.iter().any(|lock| lock.class == "OFDLCK"))
        {
            let _ = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.set_may_hold_ofd_lock());
        }
        let slots = guest.thread_state().slots_naming(open_file);
        crate::tool_global::note_description_locks(guest, open_file, locks, slots).await;
        Ok(())
    }

    /// After a lock or unlock through a descriptor whose description the
    /// ledger may know under another identity: no lock on its inode number
    /// counts from now on (`procfs::locks::LockLedger::alias_inode`). If its
    /// fdinfo could not be read for ambient or resource state, which
    /// identities it changed is unknown, and every later `/proc/locks`
    /// snapshot is refused by name (`procfs::locks::LockLedger::refuse`); if
    /// the guest's `/proc` could not be proved to be procfs, no lock counts
    /// at all (an inode of `None`), which only hides.
    async fn note_alias<G: Guest<Self>>(guest: &mut G, fdinfo: &FdinfoRead) {
        match fdinfo {
            FdinfoRead::Unknown(reason) => {
                crate::tool_global::note_lock_ledger_refused(guest, reason.clone()).await;
            }
            FdinfoRead::Contents(_) | FdinfoRead::Untrackable(_) => {
                crate::tool_global::note_aliased_inode(guest, fdinfo.inode()).await;
            }
        }
    }

    /// The descriptions named by the slots `fds` of this thread's table
    /// whose locks `procfs::locks::LockLedger` follows, taken before a close
    /// or replacement of those slots.
    fn lock_tracked_descriptions<G: Guest<Self>>(
        &self,
        guest: &G,
        fds: std::ops::RangeInclusive<RawFd>,
    ) -> Vec<OpenFileId> {
        let thread = guest.thread_state();
        let candidates = if fds.start() == fds.end() {
            vec![*fds.start()]
        } else {
            thread
                .open_fds()
                .into_iter()
                .filter(|fd| fds.contains(fd))
                .collect()
        };
        let mut tracked = candidates
            .into_iter()
            .filter_map(|fd| {
                thread
                    .with_detfd(fd, |detfd| {
                        detfd.is_lock_tracked().then(|| detfd.open_file_id())
                    })
                    .ok()
                    .flatten()
            })
            .collect::<Vec<_>>();
        tracked.sort();
        tracked.dedup();
        tracked
    }

    /// After a close or replacement of slots that named `descriptions`,
    /// report how many slots of this thread's table still name each
    /// (`procfs::locks::LockLedger::release`).
    async fn report_lock_references<G: Guest<Self>>(
        &self,
        guest: &mut G,
        descriptions: Vec<OpenFileId>,
    ) {
        for open_file in descriptions {
            let remaining = guest.thread_state().slots_naming(open_file);
            crate::tool_global::note_table_released(guest, open_file, remaining).await;
        }
    }

    /// The text of `/proc/thread-self/fdinfo/<fd>`, read through the guest's
    /// procfs only, by a recorded sequence staged in a private page mapped for
    /// this call and unmapped before it returns ([`Self::read_staged_fdinfo`]).
    ///
    /// The guest chooses its own root, so a path alone proves nothing: under a
    /// `chroot` its `/proc` can be any tree, with any text where an fdinfo is
    /// expected, and lock lines that matched a host's lock would admit the
    /// host's `/proc/locks` row. So the read starts from an anchor, the
    /// guest's `/proc`, which must be procfs (`fstatfs`), opens the entry
    /// below it with `openat2`'s `RESOLVE_BENEATH | RESOLVE_NO_XDEV` (no step
    /// out of the anchor, no mount crossed, so nothing bound over an entry),
    /// and requires the entry itself on procfs and on the anchor's device
    /// (`fstatfs` and `fstat` of both). Every step goes through the
    /// recorder, so record and replay decide alike. The read holds two
    /// descriptors at once, the anchor and the entry. These checks rely on
    /// the serialized schedule, under which no other guest thread runs
    /// between the calls; with concurrent threads the entry check narrows a
    /// `dup2` race on the descriptor numbers but does not close it
    /// ([`Self::read_opened_entry`]).
    ///
    /// No page for a guest that mapped its whole address space away, and a
    /// failed step whose errno is ambient or resource state
    /// ([`fdinfo_failure_hides`] is false), leave the description's locks
    /// unknown ([`FdinfoRead::Unknown`], whose reason names the guest's
    /// `call`, [`fdinfo_refusal`]). Every other outcome that gives no proved
    /// fdinfo (no `/proc`, not procfs, an entry outside it or across a
    /// mount, a device mismatch, a guest-decided or backend-decided errno)
    /// makes the description untrackable ([`FdinfoRead::Untrackable`]).
    async fn read_own_fdinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        call: &str,
    ) -> Result<FdinfoRead, Error> {
        let mapped = guest
            .inject_with_retry(Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(FDINFO_PAGE)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await;
        let Some(page) = mapped
            .ok()
            .and_then(|mapped| usize::try_from(mapped).ok())
            .filter(|page| *page != 0)
        else {
            return Ok(FdinfoRead::Unknown(format!(
                "after {call} on fd {fd}, Detcore could not map the page it reads that \
                 descriptor's fdinfo through ({mapped:?}), so it does not know which locks \
                 the description holds"
            )));
        };
        let result = self.read_staged_fdinfo(guest, page, fd).await;
        if let Err(errno) = guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Addr::from_raw(page))
                    .with_len(FDINFO_PAGE),
            ))
            .await
        {
            warn!(
                "[detcore] could not unmap the transient fdinfo page: {}",
                errno
            );
        }
        Ok(match result? {
            Ok(contents) => FdinfoRead::Contents(contents),
            // Ambient or resource state says nothing about the locks.
            Err(FdinfoFailure::Failed(step, errno)) if !fdinfo_failure_hides(errno) => {
                FdinfoRead::Unknown(fdinfo_refusal(call, fd, step, errno))
            }
            Err(FdinfoFailure::Failed(step, errno)) => FdinfoRead::Untrackable(format!(
                "after {call} on fd {fd}, the guest's /proc did not give a procfs fdinfo \
                 ({step}: {errno})"
            )),
            Err(FdinfoFailure::NotProcfs(why)) => FdinfoRead::Untrackable(format!(
                "after {call} on fd {fd}, the guest's /proc did not give a procfs fdinfo \
                 ({why})"
            )),
        })
    }

    /// Stop the run, by name, with the policy-refusal exit, because Detcore
    /// cannot tell a `/proc/locks` snapshot's guest rows from a host's while a
    /// description whose locks it could not read is referenced
    /// (`procfs::locks::LockLedger::unknown`).
    async fn refuse_lock_bookkeeping<G: Guest<Self>>(&self, guest: &mut G, reason: String) -> ! {
        error!("[detcore] {reason}");
        crate::tool_global::report_policy_refusal(guest, reason).await;
        crate::tool_global::unrecoverable_shutdown(guest, detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
            .await
    }

    /// [`Self::read_own_fdinfo`]'s recorded sequence, in the mapped `page`
    /// (layout: the path at [`FDINFO_PATH`], `openat2`'s `struct open_how` at
    /// [`FDINFO_HOW`], `fstatfs` and `fstat` buffers at [`FDINFO_STAT`], the
    /// read buffer from [`FDINFO_READ`]): open the anchor, prove it procfs,
    /// open the entry beneath it on its mount, compare devices, read, and
    /// close both descriptors.
    async fn read_staged_fdinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        page: usize,
        fd: RawFd,
    ) -> Result<Result<Vec<u8>, FdinfoFailure>, Error> {
        let path_ptr =
            PathPtr::from_ptr((page + FDINFO_PATH) as *const libc::c_char).ok_or(Errno::EFAULT)?;
        self.write_fdinfo_path(guest, page, "/proc")?;
        let anchor = syscalls::Openat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(Some(path_ptr))
            .with_flags(OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
            .with_mode(None);
        let anchor = match self.record_or_replay(guest, Syscall::Openat(anchor)).await {
            Ok(anchor) => anchor as RawFd,
            Err(errno) => return Ok(Err(FdinfoFailure::Failed("open", errno))),
        };
        let result = self.read_beneath_anchor(guest, page, anchor, fd).await;
        if let Err(errno) = self
            .record_or_replay(guest, syscalls::Close::new().with_fd(anchor))
            .await
        {
            trace!("[detcore] closing the fdinfo anchor failed: {errno}");
        }
        result
    }

    /// [`Self::read_staged_fdinfo`] once its `anchor` is open.
    async fn read_beneath_anchor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        page: usize,
        anchor: RawFd,
        fd: RawFd,
    ) -> Result<Result<Vec<u8>, FdinfoFailure>, Error> {
        let anchor_device = match self.recorded_procfs_device(guest, page, anchor).await? {
            Ok(device) => device,
            Err(failure) => return Ok(Err(failure)),
        };
        self.write_fdinfo_path(guest, page, &format!("thread-self/fdinfo/{fd}"))?;
        // struct open_how { u64 flags; u64 mode; u64 resolve; }
        let how = (libc::O_RDONLY | libc::O_CLOEXEC) as u64;
        let mut open_how = [0u8; FDINFO_HOW_SIZE];
        open_how[..8].copy_from_slice(&how.to_ne_bytes());
        open_how[16..].copy_from_slice(&FDINFO_RESOLVE.to_ne_bytes());
        let how_ptr = AddrMut::<u8>::from_raw(page + FDINFO_HOW).ok_or(Errno::EFAULT)?;
        guest.memory().write_exact(how_ptr, &open_how)?;
        let opened = match self
            .record_or_replay(
                guest,
                Syscall::from_raw(
                    Sysno::openat2,
                    procfs_levels::raw_args([
                        anchor as usize,
                        page + FDINFO_PATH,
                        page + FDINFO_HOW,
                        FDINFO_HOW_SIZE,
                        0,
                        0,
                    ]),
                ),
            )
            .await
        {
            Ok(opened) => opened as RawFd,
            // The entry is outside the anchor or across a mount.
            Err(Errno::EXDEV) => {
                return Ok(Err(FdinfoFailure::NotProcfs(
                    "the entry is outside /proc or across a mount",
                )));
            }
            Err(errno) => return Ok(Err(FdinfoFailure::Failed("open", errno))),
        };
        // Closed on every path, the error ones included.
        let result = self
            .read_opened_entry(guest, page, opened, anchor_device)
            .await;
        if let Err(errno) = self
            .record_or_replay(guest, syscalls::Close::new().with_fd(opened))
            .await
        {
            trace!("[detcore] closing fdinfo failed: {errno}");
        }
        result
    }

    /// The text of the `opened` fdinfo entry, once it too is shown to be on
    /// procfs and on the anchor's device. The checks run on the entry's own
    /// descriptor, so a sibling thread that replaces the anchor's descriptor
    /// number between calls (`dup2` of a fake directory) cannot pass them.
    ///
    /// The defence relies on Detcore's serialized schedule: no other guest
    /// thread runs between these injected calls. Under
    /// `--no-sequentialize-threads` a sibling could still `dup2` onto the
    /// entry's own descriptor number between its checks and the read; the
    /// entry check narrows that race but does not close it.
    async fn read_opened_entry<G: Guest<Self>>(
        &self,
        guest: &mut G,
        page: usize,
        opened: RawFd,
        anchor_device: u64,
    ) -> Result<Result<Vec<u8>, FdinfoFailure>, Error> {
        match self.recorded_procfs_device(guest, page, opened).await? {
            Ok(device) if device == anchor_device => {}
            Ok(_) => {
                return Ok(Err(FdinfoFailure::NotProcfs(
                    "the entry is not on /proc's device",
                )));
            }
            Err(failure) => return Ok(Err(failure)),
        }
        let scratch = AddrMut::<u8>::from_raw(page + FDINFO_READ).ok_or(Errno::EFAULT)?;
        match self
            .drain_procfs(
                guest,
                syscalls::Read::new().with_fd(opened),
                scratch,
                FDINFO_PAGE - FDINFO_READ,
                false,
            )
            .await
        {
            Ok(contents) => Ok(Ok(contents)),
            Err(Error::Errno(errno)) => Ok(Err(FdinfoFailure::Failed("read", errno))),
            Err(error) => Err(error),
        }
    }

    /// The device of the descriptor `fd`, once `fstatfs` shows it is on
    /// procfs, both through the recorder into the stat buffer of the fdinfo
    /// page. A failed call keeps its errno, for the common classification
    /// ([`fdinfo_failure_hides`]).
    async fn recorded_procfs_device<G: Guest<Self>>(
        &self,
        guest: &mut G,
        page: usize,
        fd: RawFd,
    ) -> Result<Result<u64, FdinfoFailure>, Error> {
        let stat = page + FDINFO_STAT;
        if let Err(errno) = self
            .record_or_replay(
                guest,
                Syscall::from_raw(
                    Sysno::fstatfs,
                    procfs_levels::raw_args([fd as usize, stat, 0, 0, 0, 0]),
                ),
            )
            .await
        {
            return Ok(Err(FdinfoFailure::Failed("fstatfs", errno)));
        }
        // `f_type` is the first field of `struct statfs`.
        if self.read_fdinfo_word(guest, stat)? != libc::PROC_SUPER_MAGIC as u64 {
            return Ok(Err(FdinfoFailure::NotProcfs("not procfs")));
        }
        if let Err(errno) = self
            .record_or_replay(
                guest,
                Syscall::from_raw(
                    Sysno::fstat,
                    procfs_levels::raw_args([fd as usize, stat, 0, 0, 0, 0]),
                ),
            )
            .await
        {
            return Ok(Err(FdinfoFailure::Failed("fstat", errno)));
        }
        // `st_dev` is the first field of `struct stat`.
        Ok(Ok(self.read_fdinfo_word(guest, stat)?))
    }

    /// Write `path`, NUL-terminated, at the fdinfo page's path slot.
    fn write_fdinfo_path<G: Guest<Self>>(
        &self,
        guest: &mut G,
        page: usize,
        path: &str,
    ) -> Result<(), Error> {
        assert!(
            path.len() < FDINFO_HOW - FDINFO_PATH,
            "fdinfo path {path:?} outgrew its room"
        );
        let start = AddrMut::<u8>::from_raw(page + FDINFO_PATH).ok_or(Errno::EFAULT)?;
        let mut bytes = path.as_bytes().to_vec();
        bytes.push(0);
        guest.memory().write_exact(start, &bytes)?;
        Ok(())
    }

    /// The first 8 bytes at `address`, a native-endian `u64`.
    fn read_fdinfo_word<G: Guest<Self>>(
        &self,
        guest: &mut G,
        address: usize,
    ) -> Result<u64, Error> {
        let mut word = [0u8; 8];
        let start = Addr::<u8>::from_raw(address).ok_or(Errno::EFAULT)?;
        guest.memory().read_exact(start, &mut word)?;
        Ok(u64::from_ne_bytes(word))
    }

    /// Refuse with EPERM a call that would make one of Hermit's tasks the
    /// owner of a descriptor's signals (https://github.com/rrnewton/hermit/issues/4083).
    /// Linux would accept it and later signal Hermit whenever the descriptor is
    /// ready; with `F_SETSIG(SIGTERM)` that ended Hermit's run. Refusing is a
    /// policy, the answer `pidfd_open` of Hermit's process gets.
    async fn refuse_hermit_owner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        command: &str,
        owner: libc::c_int,
    ) -> Result<(), Error> {
        if owner <= 0 || !crate::tool_global::names_hermit_task(guest, None, owner).await {
            return Ok(());
        }
        tracing::info!(
            "[detcore, dtid {}] {command} on fd {fd} names task {owner}, one of Hermit's own, \
             as the owner of its signals: refused with EPERM \
             (https://github.com/rrnewton/hermit/issues/4083)",
            guest.thread_state().dettid,
        );
        Err(Errno::EPERM.into())
    }

    /// `call`, unless it is an `fcntl` that sets a descriptor's signal owner
    /// to one of Hermit's tasks ([`Self::refuse_hermit_owner`]): `F_SETOWN`
    /// with a positive pid, or `F_SETOWN_EX` naming a thread (`F_OWNER_TID`)
    /// or a process (`F_OWNER_PID`).
    ///
    /// `F_SETOWN` passes the owner by value; it is read from the raw argument,
    /// because the pinned Reverie's typed `FcntlCmd::F_SETOWN` carries none.
    /// `F_SETOWN_EX` passes it through guest memory, which another guest
    /// thread could change between this check and the kernel's own read, so
    /// the `f_owner_ex` is read once and the kernel is handed Detcore's copy,
    /// on this thread's scratch stack. An `f_owner_ex` that cannot be read is
    /// EFAULT here, as the kernel would answer, rather than a pointer passed
    /// on for the kernel to read later. The returned guard keeps the copy
    /// alive (SaBRe's scratch stack is an arena it owns) and must be held
    /// until the call has run.
    async fn screen_fcntl_owner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Fcntl,
    ) -> Result<(syscalls::Fcntl, Option<<G::Stack as Stack>::StackGuard>), Error> {
        // Linux's include/uapi/asm-generic/fcntl.h; the pinned libc lacks them.
        const F_OWNER_TID: libc::c_int = 0;
        const F_OWNER_PID: libc::c_int = 1;
        match call.cmd() {
            F_SETOWN => {
                let (_, args) = call.into_parts();
                self.refuse_hermit_owner(guest, call.fd(), "F_SETOWN", args.arg2 as libc::c_int)
                    .await?;
                Ok((call, None))
            }
            F_SETOWN_EX(owner) => {
                let owner: Addr<[libc::c_int; 2]> = owner.ok_or(Errno::EFAULT)?.cast();
                let copy = guest.memory().read_value(owner)?;
                if matches!(copy[0], F_OWNER_TID | F_OWNER_PID) {
                    self.refuse_hermit_owner(guest, call.fd(), "F_SETOWN_EX", copy[1])
                        .await?;
                }
                let mut stack = guest.stack().await;
                let copied: Addr<[libc::c_int; 2]> = stack.push(copy);
                let guard = stack.commit()?;
                Ok((call.with_cmd(F_SETOWN_EX(Some(copied.cast()))), Some(guard)))
            }
            _ => Ok((call, None)),
        }
    }

    /// `call`, unless it is an `ioctl` that sets a descriptor's signal owner
    /// to one of Hermit's tasks ([`Self::refuse_hermit_owner`]): `FIOSETOWN`
    /// or `SIOCSPGRP` with a positive pid, which set the owner as `F_SETOWN`
    /// does. The owner is passed through guest memory, so, as for
    /// `F_SETOWN_EX` ([`Self::screen_fcntl_owner`]), it is read once and the
    /// kernel is handed Detcore's copy, kept alive by the returned guard;
    /// an unreadable one is EFAULT.
    async fn screen_ioctl_owner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ioctl,
    ) -> Result<(syscalls::Ioctl, Option<<G::Stack as Stack>::StackGuard>), Error> {
        let (command, owner) = match call.request() {
            syscalls::ioctl::Request::FIOSETOWN(owner) => ("FIOSETOWN", owner),
            syscalls::ioctl::Request::SIOCSPGRP(owner) => ("SIOCSPGRP", owner),
            _ => return Ok((call, None)),
        };
        let copy: libc::c_int = guest.memory().read_value(owner.ok_or(Errno::EFAULT)?)?;
        self.refuse_hermit_owner(guest, call.fd(), command, copy)
            .await?;
        let mut stack = guest.stack().await;
        let copied = stack.push(copy);
        let guard = stack.commit()?;
        let request = match call.request() {
            syscalls::ioctl::Request::FIOSETOWN(_) => {
                syscalls::ioctl::Request::FIOSETOWN(Some(copied))
            }
            _ => syscalls::ioctl::Request::SIOCSPGRP(Some(copied)),
        };
        Ok((call.with_request(request), Some(guard)))
    }

    /// ioctl system call
    pub async fn handle_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ioctl,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        // Setting a virtual timerfd's host tick count would make the host
        // disagree with Detcore's count (`crate::syscalls::timerfd`).
        if Syscall::Ioctl(call).into_parts().1.arg1 as u64
            == crate::syscalls::timerfd::TFD_IOC_SET_TICKS
            && crate::syscalls::timerfd::enabled(guest)
            && crate::syscalls::timerfd::is_timerfd(guest, fd)
        {
            crate::syscalls::timerfd::refuse(format_args!(
                "ioctl(TFD_IOC_SET_TICKS) on timerfd {fd} would set the host's tick count"
            ));
        }
        // Before anything else, so a refused call has no other effect.
        let (call, _owner_copy) = self.screen_ioctl_owner(guest, call).await?;
        // As in `handle_fcntl`: an asynchronous-I/O owner, and a session whose
        // controlling terminal hangs up, are signalled at a moment set by host
        // timing, so hold their signals in every process.
        let signals = ioctl_host_timed_signals(call.request());
        if signals != 0 && guest.config().backend.host_timed_signals_are_physical {
            record_host_timed_signals(guest, HostTimedSignalScope::Container, signals).await;
        }
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
            let (fd_type, socketpair_endpoint, physically_nonblocking, inherited_external_read) =
                guest.thread_state().with_detfd(fd, |detfd| {
                    (
                        detfd.ty(),
                        detfd.is_socketpair_endpoint(),
                        detfd.physically_nonblocking(),
                        detfd.inherited_external_read().is_some(),
                    )
                })?;
            if self.keeps_physically_nonblocking(
                fd_type,
                socketpair_endpoint,
                physically_nonblocking,
                inherited_external_read,
            ) && physically_nonblocking
            {
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
        if let Some(pointer) = call.path() {
            let path: PathBuf = pointer.read(&guest.memory())?;
            let observed = Self::observed_path(guest, libc::AT_FDCWD, &path)?;
            if self.rendered_procfs_at(guest, &observed).await?.is_some()
                || self
                    .placeholder_reached(guest, libc::AT_FDCWD, Some(pointer), true)
                    .await
                    .is_some()
            {
                self.write_procfs_statfs(guest, call.buf())?;
                return Ok(0);
            }
        }
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
        let rendered = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_rendered_path().is_some())
            .unwrap_or(false);
        if rendered {
            self.write_procfs_statfs(guest, call.buf())?;
            return Ok(0);
        }
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
        let lock_tracked = self.lock_tracked_descriptions(guest, new_fd..=new_fd);
        let replaced = guest
            .thread_state_mut()
            .dup_fd(old_fd, new_fd, OFlag::empty())?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        self.report_lock_references(guest, lock_tracked).await;
        Ok(new_fd as i64)
    }

    /// dup2 system call.
    pub async fn handle_dup2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup2,
    ) -> Result<i64, Error> {
        let old_fd = call.oldfd();
        let new_fd = call.newfd();
        if let Some(refusal) = self
            .refuse_dup_over_waited_listener(guest, Sysno::dup2, old_fd, new_fd)
            .await
        {
            return refusal;
        }
        // `dup2` over another open descriptor closes it.
        let closed_end = (old_fd != new_fd)
            .then(|| self.pipe_channel_end(guest, new_fd))
            .flatten();
        let res = self.record_or_replay(guest, call).await?;
        // Reported as soon as the kernel replaced `new_fd`, before Detcore's
        // own table update, which can still fail for a descriptor it does not
        // track (`Scheduler::note_descriptors_replaced`).
        if old_fd != new_fd
            && let Ok(new_fd) = u32::try_from(new_fd)
        {
            crate::tool_global::descriptors_replaced(guest, new_fd, new_fd).await;
        }
        let lock_tracked = if old_fd == new_fd {
            Vec::new()
        } else {
            self.lock_tracked_descriptions(guest, new_fd..=new_fd)
        };
        let replaced = self
            .dup_fd_after_replacement(guest, old_fd, new_fd, OFlag::empty(), lock_tracked.clone())
            .await?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        self.report_lock_references(guest, lock_tracked).await;
        self.note_channel_end_closed(guest, closed_end).await;
        Ok(res)
    }

    /// Detcore's table update after the kernel's `dup2` or `dup3` put
    /// `old_fd`'s description in slot `new_fd`. If Detcore keeps no model of
    /// `old_fd` (one received over `SCM_RIGHTS` on ptrace), the update fails,
    /// but the kernel replaced the slot all the same: its old description is
    /// dropped from the model and `lock_tracked` (what the slot named) is
    /// reported to `procfs::locks::LockLedger`, so no stale entry can admit
    /// a host's identical `/proc/locks` row. The error is returned as before.
    async fn dup_fd_after_replacement<G: Guest<Self>>(
        &self,
        guest: &mut G,
        old_fd: RawFd,
        new_fd: RawFd,
        flags: OFlag,
        lock_tracked: Vec<OpenFileId>,
    ) -> Result<Option<OpenFileId>, Error> {
        match guest.thread_state_mut().dup_fd(old_fd, new_fd, flags) {
            Ok(replaced) => Ok(replaced),
            Err(errno) => {
                if old_fd != new_fd {
                    let displaced = guest.thread_state().remove_fd(new_fd);
                    if let Some(open_file_id) = displaced {
                        self.release_port_for_open_file(guest, open_file_id).await;
                    }
                    self.report_lock_references(guest, lock_tracked).await;
                }
                Err(errno.into())
            }
        }
    }

    /// Refuse a `dup2` or `dup3` that would close `new_fd` while a blocking
    /// accept waits on it. With `old_fd == new_fd` neither call closes
    /// anything. The check comes before the call, so a call that would fail
    /// for another reason (a bad `old_fd`) is refused as well.
    async fn refuse_dup_over_waited_listener<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        old_fd: RawFd,
        new_fd: RawFd,
    ) -> Option<Result<i64, Error>> {
        if old_fd == new_fd {
            return None;
        }
        let new_fd = u32::try_from(new_fd).ok()?;
        self.refuse_close_of_waited_listener(guest, sysno, new_fd..=new_fd)
            .await
    }

    /// dup3 system call.
    pub async fn handle_dup3<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup3,
    ) -> Result<i64, Error> {
        let old_fd = call.oldfd();
        let new_fd = call.newfd();
        let flags = call.flags();
        if let Some(refusal) = self
            .refuse_dup_over_waited_listener(guest, Sysno::dup3, old_fd, new_fd)
            .await
        {
            return refusal;
        }
        // `dup3` over another open descriptor closes it.
        let closed_end = (old_fd != new_fd)
            .then(|| self.pipe_channel_end(guest, new_fd))
            .flatten();
        let res = self.record_or_replay(guest, call).await?;
        // Reported before Detcore's own table update, as for `dup2`.
        if let Ok(new_fd) = u32::try_from(new_fd) {
            crate::tool_global::descriptors_replaced(guest, new_fd, new_fd).await;
        }
        let lock_tracked = self.lock_tracked_descriptions(guest, new_fd..=new_fd);
        let replaced = self
            .dup_fd_after_replacement(guest, old_fd, new_fd, flags, lock_tracked.clone())
            .await?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        self.report_lock_references(guest, lock_tracked).await;
        self.note_channel_end_closed(guest, closed_end).await;
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
            // Both ends name the pipe by the read end's open file description,
            // for `--pipe-wake-latency` only (`crate::pipe_wake`).
            if self.cfg.pipe_wake_latency_in_effect().is_some() {
                let read_end = guest
                    .thread_state()
                    .with_detfd(fds[0], |detfd| detfd.open_file_id())?;
                let channel = crate::pipe_wake::ChannelId::Pipe(read_end);
                for (fd, reads) in [(fds[0], true), (fds[1], false)] {
                    let end = crate::pipe_wake::ChannelEnd {
                        channel,
                        reads,
                        writes: !reads,
                    };
                    guest
                        .thread_state()
                        .with_detfd(fd, |detfd| detfd.set_channel(end))?;
                }
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
        let times = match call.times() {
            None => {
                let now: Timespec = thread_observe_time(guest).await.into();
                [now, now]
            }
            Some(times) => {
                let utimbuf = guest.memory().read_value(times)?;
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

        let utimensat = syscalls::Utimensat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(call.path());

        self.set_file_times(guest, utimensat, Some(times)).await
    }

    /// utimes syscall
    pub async fn handle_utimes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimes,
    ) -> Result<i64, Errno> {
        let utimensat = syscalls::Utimensat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(call.filename());

        match call.times() {
            None => {
                let now: Timespec = thread_observe_time(guest).await.into();
                self.set_file_times(guest, utimensat, Some([now, now]))
                    .await
            }
            Some(times) => {
                // Convert the timeval array to a timespec array.
                let mut memory = guest.memory();
                let tvs = memory.read_value(times)?;
                let tp: Addr<[Timespec; 2]> = times.cast();

                // Safety: The address could point to read-only memory and the
                // write below could fail.
                let tp = unsafe { tp.into_mut() };

                memory.write_value(tp, &[tvs[0].into(), tvs[1].into()])?;
                self.set_file_times(guest, utimensat.with_times(Some(tp.into())), None)
                    .await
            }
        }
    }

    /// ustimensat syscall
    pub async fn handle_utimensat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimensat,
    ) -> Result<i64, Errno> {
        self.set_file_times(guest, call, None).await
    }

    /// Performs a utimensat call and copies the mtime it sets into the virtual
    /// mtime. With `staged` the times are first pushed onto the guest's scratch
    /// stack and replace the call's `times` pointer; utime and utimes(NULL)
    /// build their times that way.
    async fn set_file_times<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimensat,
        staged: Option<[Timespec; 2]>,
    ) -> Result<i64, Errno> {
        if !guest.config().virtualize_metadata {
            return self.utimensat_without_lookup(guest, call, staged).await;
        }
        // Without thread sequentialization another guest thread can swap the
        // target away and back around the call, so that both lookups name a
        // file the kernel did not update. No lookup can tell the two apart,
        // so the virtual mtime is left alone.
        if !guest.config().sequentialize_threads {
            return self.utimensat_without_lookup(guest, call, staged).await;
        }
        // The scratch stack starts 128 bytes below the stack pointer and its
        // addresses are computed by subtraction, so a stack pointer too close
        // to zero to hold the staged times and the lookup buffer would stop
        // Hermit rather than reach Linux.
        let scratch = 128
            + staged.map_or(0, |_| std::mem::size_of::<[Timespec; 2]>())
            + std::mem::size_of::<libc::stat>();
        if usize::try_from(guest.regs().await.rsp).map_or(true, |rsp| rsp <= scratch) {
            info!(
                "Guest stack pointer cannot hold the utimensat target lookup; \
                 leaving the virtual mtime unchanged."
            );
            return self.utimensat_without_lookup(guest, call, staged).await;
        }

        // The staged times and the buffer for the target lookups share one
        // scratch stack, and its guard is held until the last injected syscall
        // has run, so that neither the kernel's read of the times nor a lookup
        // finds the other's bytes, or a restored stack, at its address.
        let mut stack = guest.stack().await;
        let staged_call = match staged {
            Some(times) => call.with_times(Some(stack.push(times))),
            None => call,
        };
        let statptr: StatPtr = StatPtr(stack.reserve());
        // The scratch stack lies below the guest's red zone, where a raw
        // syscall may keep its own path or times. Writing the lookup buffer
        // over either would change the call before Linux reads it.
        if utimensat_input_overlaps(&guest.memory(), &call, staged.is_none(), statptr) {
            info!(
                "utimensat inputs overlap the target lookup buffer; \
                 leaving the virtual mtime unchanged."
            );
            drop(stack);
            return self.utimensat_without_lookup(guest, call, staged).await;
        }
        // The buffer's address range can still share its pages with guest data
        // through a second shared mapping, which no address comparison sees.
        // Its bytes are saved here and put back after the commit and after
        // each lookup, so the lookups and the call read their inputs unchanged
        // and the guest keeps its memory.
        let mut saved = [0u8; std::mem::size_of::<libc::stat>()];
        if guest
            .memory()
            .read_exact(statptr.0.cast(), &mut saved)
            .is_err()
        {
            info!(
                "Guest stack scratch cannot hold the utimensat target lookup; \
                 leaving the virtual mtime unchanged."
            );
            drop(stack);
            return self.utimensat_without_lookup(guest, call, staged).await;
        }
        let _guard = match stack.commit() {
            Ok(guard) => guard,
            // The lookup buffer only serves the virtual update, so a scratch
            // stack that cannot hold it, for example one next to the guard
            // page, must not keep the guest's own call from reaching Linux.
            // The commit writes page by page, so it can fail after writing a
            // lower writable page; those bytes are put back first.
            Err(_) => {
                info!(
                    "Guest stack scratch cannot hold the utimensat target lookup; \
                     leaving the virtual mtime unchanged."
                );
                restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
                return self.utimensat_without_lookup(guest, call, staged).await;
            }
        };
        restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
        let call = staged_call;

        // The kernel applies the new times to the real file, but the guest
        // observes the virtual mtime, which otherwise only moves on writes. Copy
        // the requested mtime into it so that `tar` extraction, `cp -p` and
        // `touch -r` restore a file's mtime and `make` compares the times the
        // build asked for rather than the order in which files were unpacked.
        //
        // Nothing below may change the syscall's result: an unreadable `times`
        // or a failed lookup only skips the virtual update, and the kernel
        // reports its own error for the call itself.
        let mtime = match (staged, call.times()) {
            (Some([_, mtime]), _) => Some(mtime),
            (None, None) => Some(Timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW,
            }),
            (None, Some(times)) => guest
                .memory()
                .read_value(times)
                .ok()
                .map(|[_, mtime]| mtime),
        }
        .filter(|mtime| mtime.tv_nsec != libc::UTIME_OMIT);
        let before = match mtime {
            Some(_) => self.utimensat_target(guest, &call, statptr).await,
            None => None,
        };
        restore_lookup_buffer(&mut guest.memory(), statptr, &saved);

        let res = self.record_or_replay(guest, call).await?;

        // Update only the inode the kernel modified. No other guest thread
        // runs during the call, but a process outside the container can still
        // rename, unlink or replace the target; the target is resolved before
        // and after the call, and the update is skipped unless both name the
        // same file. Equal lookups alone do not show that the kernel updated
        // that file: the name may have been swapped away and back during the
        // call, or an inode number reused. So an explicit mtime is copied only
        // when the file holds it, truncated to the filesystem's granularity,
        // and the virtual mtime takes the value the kernel stored, which is
        // what stat reports on Linux. `UTIME_NOW` has no such witness.
        let (Some(mtime), Some(before)) = (mtime, before) else {
            return Ok(res);
        };
        let after = self.utimensat_target(guest, &call, statptr).await;
        restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
        let Some(after) = after else {
            return Ok(res);
        };
        if (after.st_dev, after.st_ino) != (before.st_dev, before.st_ino) {
            return Ok(res);
        }
        if mtime.tv_nsec == libc::UTIME_NOW {
            touch_file(guest, RawInode::new(after.st_dev, after.st_ino)).await;
            return Ok(res);
        }
        const NANOS_PER_SEC: i128 = 1_000_000_000;
        let requested = i128::from(mtime.tv_sec) * NANOS_PER_SEC + i128::from(mtime.tv_nsec);
        let stored = i128::from(after.st_mtime) * NANOS_PER_SEC + i128::from(after.st_mtime_nsec);
        // Linux truncates the requested time down to the filesystem's
        // granularity, at most a second on the filesystems builds use.
        if (0..NANOS_PER_SEC).contains(&(requested - stored)) {
            let nanos = u64::try_from(stored.max(0)).unwrap_or(u64::MAX);
            set_file_mtime(
                guest,
                RawInode::new(after.st_dev, after.st_ino),
                LogicalTime::from_nanos(nanos),
            )
            .await;
        }
        Ok(res)
    }

    /// Performs a utimensat call without the virtual mtime update. Staged
    /// times are then the only scratch-stack allocation, as they were before
    /// the update existed.
    async fn utimensat_without_lookup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimensat,
        staged: Option<[Timespec; 2]>,
    ) -> Result<i64, Errno> {
        let Some(times) = staged else {
            return self.record_or_replay(guest, call).await;
        };
        let mut stack = guest.stack().await;
        let call = call.with_times(Some(stack.push(times)));
        let _guard = stack.commit()?;
        self.record_or_replay(guest, call).await
    }

    /// The metadata of the file a utimensat call targets, with the same target
    /// selection: the descriptor itself for `futimens` (a NULL path), else a
    /// path walk honoring the flags utimensat accepts. `None` if the lookup
    /// fails.
    async fn utimensat_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &syscalls::Utimensat,
        statptr: StatPtr<'_>,
    ) -> Option<libc::stat> {
        let lookup = match call.path() {
            None => Syscall::Fstat(
                syscalls::Fstat::new()
                    .with_fd(call.dirfd())
                    .with_stat(Some(statptr)),
            ),
            Some(path) => {
                let allowed = libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH;
                Syscall::Newfstatat(
                    syscalls::Newfstatat::new()
                        .with_dirfd(call.dirfd())
                        .with_path(Some(path))
                        .with_stat(Some(statptr))
                        .with_flags(AtFlags::from_bits_truncate(call.flags() & allowed)),
                )
            }
        };
        self.record_or_replay(guest, lookup).await.ok()?;
        statptr.read(&guest.memory()).ok()
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
            // Linux's SOCK_TYPE_MASK: the type without SOCK_NONBLOCK and SOCK_CLOEXEC.
            const SOCK_TYPE_MASK: i32 = 0xf;
            if self.accept_in_turn_enabled() && call.r#type() & SOCK_TYPE_MASK == libc::SOCK_STREAM
            {
                guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.set_accept_in_turn_eligible())?;
            }
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

    // TODO-HUMAN-REVIEW(PR-3908)
    /// Whether record/replay runs a blocking `accept(2)` on a stream socket
    /// the container created in the caller's turn
    /// (https://github.com/rrnewton/hermit/issues/3880). The temporary
    /// `O_NONBLOCK` around each attempt goes through Detcore's own copy of the
    /// listener (`AcceptBracket`), which needs a tool outside the guest's
    /// descriptor table, as under ptrace.
    pub(crate) fn accept_in_turn_enabled(&self) -> bool {
        self.cfg.recordreplay_modes
            && self.cfg.use_nonblocking_sockets()
            && !self.cfg.backend.tool_shares_guest_descriptor_table
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

            // Both endpoints are container-internal by construction: socketpair(2)
            // creates the pair itself, so no peer outside the container can ever
            // hold one. Record that here, at the only point where it is known,
            // so potentially-blocking I/O on them takes the same
            // nonblockize-and-retry path as a pipe rather than being treated as
            // external blocking I/O against an fd Detcore just made SOCK_NONBLOCK.
            // See `syscall_targets_internal_fd`. Only when the endpoints really
            // are physically nonblocking, which is the same condition as `call2`
            // above: under `debug_externalize_sockets` they stay external.
            if self.cfg.use_nonblocking_sockets() {
                for fd in fds {
                    let _ = guest
                        .thread_state()
                        .with_detfd(fd, |detfd| detfd.set_socketpair_endpoint());
                }
            }

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
    ///
    /// Under record and replay a successful `SO_RCVTIMEO` also sets the
    /// socket's modeled receive timeout (`DetFd::receive_timeout`), which a
    /// blocking accept in turn waits by. The value is read before the call,
    /// from the same guest memory in both phases.
    pub async fn handle_setsockopt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setsockopt,
    ) -> Result<i64, Error> {
        // TODO-HUMAN-REVIEW(PR-3908)
        const SO_RCVTIMEO_NEW: i32 = 66;
        let receive_timeout = (self.accept_in_turn_enabled()
            && call.level() == libc::SOL_SOCKET
            && matches!(call.optname(), libc::SO_RCVTIMEO | SO_RCVTIMEO_NEW))
        .then(|| {
            // On x86_64 both forms take 16 bytes: seconds, then microseconds.
            call.optval()
                .and_then(|value| {
                    guest
                        .memory()
                        .read_value::<_, libc::timeval>(value.cast())
                        .ok()
                })
                .map_or(ReceiveTimeout::Unknown, ReceiveTimeout::set_by)
        });
        let result = self.record_or_replay(guest, call).await?;
        if let Some(timeout) = receive_timeout {
            let _ = guest
                .thread_state()
                .with_detfd(call.fd(), |detfd| detfd.set_receive_timeout(timeout));
        }
        Ok(result)
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
        // A signalfd of a pending real-time timer expiration that every thread
        // blocked when it expired would be readable at once
        // (https://github.com/rrnewton/hermit/issues/4089). A mask the kernel
        // cannot read fails the call below.
        let mask = match call.mask().and_then(|mask| Addr::from_raw(mask.as_raw())) {
            Some(mask) => {
                // The call's first injection (`restart_if_first_probe_not_run`).
                match crate::syscalls::signal::read_kernel_sigset(guest, mask).await {
                    Err(error @ Error::Errno(Errno::ERESTARTSYS)) => {
                        return crate::syscalls::signal::restart_if_first_probe_not_run(Err(error));
                    }
                    read => read.ok(),
                }
            }
            None => None,
        };
        if let Some(mask) = mask {
            let pending = crate::syscalls::helpers::pending_real_time_among(guest, mask).await;
            crate::syscalls::helpers::act_on_pending_real_time(guest, pending, mask, 0).await;
        }
        let signalfd = self.record_or_replay(guest, call).await? as RawFd;
        // A real-time timer expiration this descriptor, or a copy a fork
        // inherits, could read is never kept pending
        // (`Scheduler::real_time_expiration_fate`), a `SIGEV_THREAD_ID`
        // timer's copy of a signal it covers may be taken unobserved
        // (`Scheduler::thread_timer_copy_is_observable`), and a process one
        // of whose threads blocks in an I/O call while it holds one for
        // SIGCHLD does not attribute SIGCHLD through the send window
        // (`Scheduler::note_outside_blocker`), since that call may read it
        // outside the schedule.
        if let Some(mask) = mask
            && mask != 0
            && let Some(detpid) = guest.thread_state().detpid
        {
            crate::tool_global::register_signalfd(guest, detpid, mask).await;
        }
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
        // A timerfd on virtual time is physically nonblocking, as an eventfd
        // is, so a logically blocking read retries in scheduler turns
        // (internal IO polling) instead of blocking in the host until a
        // host-timed moment (`crate::syscalls::timerfd`).
        crate::syscalls::timerfd::refuse_unshared_model(guest);
        let internally_nonblocking = crate::syscalls::timerfd::enabled(guest);
        let injected = if internally_nonblocking {
            call.with_flags(call.flags() | syscalls::TimerFlags::TFD_NONBLOCK)
        } else {
            call
        };
        let fd = self.record_or_replay(guest, injected).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::TFD_CLOEXEC | libc::TFD_NONBLOCK),
            ),
            FdType::Timerfd,
        )
        .await?;
        if internally_nonblocking {
            self.maybe_set_nonblocking_fd(guest, fd);
        }
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

    /// timerfd_settime system call. Under virtual time the setting is kept on
    /// Detcore's clock and the host timerfd stays disarmed
    /// (`crate::syscalls::timerfd`).
    pub async fn handle_timerfd_settime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdSettime,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await;
        if let Some(result) = crate::syscalls::timerfd::settime(guest, call).await? {
            return Ok(result);
        }
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// timerfd_gettime system call, answered from the virtual clock for a
    /// timerfd Detcore keeps there.
    pub async fn handle_timerfd_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdGettime,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await;
        if let Some(result) = crate::syscalls::timerfd::gettime(guest, call).await? {
            return Ok(result);
        }
        Ok(self.record_or_replay(guest, call).await?)
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
        // A pidfd for Hermit's own process, PID 1 of the guest's namespace,
        // would let `pidfd_send_signal` signal it and `pidfd_getfd` take its
        // descriptors (https://github.com/rrnewton/hermit/issues/4066). Linux
        // would open it; refusing it with EPERM is a policy, the answer the
        // guest already gets for `ptrace` and `process_vm_readv` on that process.
        let pid = call.pid() as i32;
        // A nested caller names its own namespace's pid, where none of
        // Hermit's tasks is visible; the pidfd records the task it names
        // (https://github.com/rrnewton/hermit/issues/4155). A pid no task of
        // that namespace has goes to the kernel, which answers ESRCH.
        if pid > 0 && guest.thread_state().nested_pid_namespace {
            let target = match crate::tool_global::guest_pid(guest, pid).await {
                crate::scheduler::pid_namespaces::GuestPidAnswer::Task { task, .. } => Some(task),
                crate::scheduler::pid_namespaces::GuestPidAnswer::NoSuchPid => None,
                crate::scheduler::pid_namespaces::GuestPidAnswer::Untranslatable(reason) => {
                    tracing::error!(
                        "[detcore, dtid {}] pidfd_open of pid {} from a nested PID namespace is not supported: {} (https://github.com/rrnewton/hermit/issues/4155)",
                        guest.thread_state().dettid,
                        pid,
                        reason,
                    );
                    return self
                        .refuse_unserviceable_operation(guest, Sysno::pidfd_open, Errno::ENOSYS)
                        .await;
                }
            };
            let fd = self.record_or_replay(guest, call).await? as RawFd;
            let flags = OFlag::O_CLOEXEC | OFlag::from_bits_truncate(call.flags() as libc::c_int);
            self.add_fd(guest, fd, flags, FdType::Pidfd).await?;
            if let Some(target) = target {
                guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.set_pidfd_target(target))?;
            }
            return Ok(fd as i64);
        }
        if pid > 0 && crate::tool_global::names_hermit_task(guest, Some(pid), pid).await {
            tracing::info!(
                "[detcore, dtid {}] pidfd_open of {}, Hermit's own process: refused with EPERM \
                 (https://github.com/rrnewton/hermit/issues/4066)",
                guest.thread_state().dettid,
                pid,
            );
            return Err(Errno::EPERM.into());
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
    /// generation runs inside this thread's serialized scheduler turn, as for
    /// `tgkill`/`tkill`/`rt_tgsigqueueinfo`, which forward through record/replay
    /// and then tell the scheduler that the signal is pending for its receiver
    /// (`notify_cross_task_signal`), so that a receiver parked in a blocking call
    /// is woken. This handler does the same for a process with one live thread
    /// (<https://github.com/rrnewton/hermit/issues/4099>).
    ///
    /// The flags and the descriptor are decided as Linux decides them, ahead of
    /// the forward (<https://github.com/rrnewton/hermit/issues/4108>):
    /// - an unknown flag bit, or more than one of the three scope flags, is
    ///   EINVAL; `PIDFD_SIGNAL_THREAD_GROUP` is the scope a process pidfd has
    ///   anyway, and `PIDFD_SIGNAL_THREAD` sends to the thread whose id is the
    ///   pid, as `tgkill(pid, pid)` does; `PIDFD_SIGNAL_PROCESS_GROUP` is
    ///   refused by name, even with signal 0, whose answer would come from the
    ///   process group the guest inherits from Hermit;
    /// - a descriptor names a process when it is a pidfd from `pidfd_open` or
    ///   a `/proc/<pid>` directory of the guest's procfs
    ///   (`process_directory_descriptor`); any other file is EBADF, and a
    ///   descriptor Detcore cannot classify is refused by name.
    ///
    /// A process-directed signal to a process with several live threads is
    /// refused by name, as `kill` refuses it (`Detcore::process_signal_target`).
    ///
    /// `call` is the raw `Syscall::Other`; `pidfd`/`flags` are pre-extracted from
    /// its arguments by the dispatcher.
    pub async fn handle_pidfd_send_signal<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        pidfd: RawFd,
        signal: i32,
        flags: u32,
    ) -> Result<i64, Error> {
        // Linux checks the flags before it looks at the descriptor.
        let mut scope = pidfd_signal_scope(flags)?;
        let info = AddrMut::<GuestSiginfo>::from_raw(call.into_parts().1.arg2);
        if let Some(refused) = self
            .refuse_forged_own_sigchld(guest, Sysno::pidfd_send_signal, signal, info)
            .await
        {
            return refused;
        }
        // The two sentinels name the caller without a descriptor: its thread
        // (`PIDFD_SELF_THREAD`, thread-directed unless a flag says otherwise)
        // or its thread group (`PIDFD_SELF_THREAD_GROUP`)
        // (https://github.com/rrnewton/hermit/issues/4147).
        let named = if let Some((target, default)) = pidfd_self_target(guest, pidfd) {
            if flags == 0 {
                scope = default;
            }
            Some(target)
        } else {
            // An unknown or closed descriptor is EBADF, and so is any file that is
            // neither a pidfd nor an opened path (a pipe, a socket, an eventfd...).
            // Linux's fdget skips an O_PATH descriptor, so it is EBADF too.
            let kind = guest
                .thread_state()
                .with_detfd(pidfd, |detfd| match detfd.ty() {
                    FdType::Pidfd => Some(Some(detfd.pidfd_target())),
                    FdType::Regular if detfd.status_flags() & libc::O_PATH == 0 => Some(None),
                    _ => None,
                })?;
            let Some(kind) = kind else {
                return Err(Errno::EBADF.into());
            };
            // `named` is the pid the descriptor names. A pidfd from `pidfd_open`
            // names a process leader; a `/proc/<tid>` directory may name any thread,
            // whose process is its leader's (https://github.com/rrnewton/hermit/issues/4094).
            match kind {
                Some(target) => target,
                None => {
                    let descriptor = if self.cfg.recordreplay_modes {
                        // Replay holds a placeholder for a reopened file, so the
                        // descriptor cannot be read the same way in both.
                        ProcessDirectoryDescriptor::Unknown(
                            "record and replay do not read what a descriptor names",
                        )
                    } else if !guest.config().backend.host_task_reads_reach_guest_threads {
                        // Only on ptrace are the guest's thread ids the host's, so
                        // that its descriptors can be read through /proc/<tid>.
                        ProcessDirectoryDescriptor::Unknown(
                            "this backend's guest thread ids are not host thread ids",
                        )
                    } else {
                        process_directory_descriptor(guest.tid().as_raw(), pidfd)
                    };
                    match descriptor {
                        ProcessDirectoryDescriptor::Process(task)
                            if crate::tool_global::names_hermit_task(
                                guest,
                                None,
                                task.as_raw(),
                            )
                            .await =>
                        {
                            // Any of Hermit's threads, as a `kill` naming one is
                            // dropped and `pidfd_open` of Hermit's process is refused
                            // (https://github.com/rrnewton/hermit/issues/4066).
                            tracing::info!(
                                "[detcore, dtid {}] pidfd_send_signal through /proc/{}, one of \
                                 Hermit's own threads: refused with EPERM",
                                guest.thread_state().dettid,
                                task,
                            );
                            return Err(Errno::EPERM.into());
                        }
                        ProcessDirectoryDescriptor::Process(task) => Some(task),
                        ProcessDirectoryDescriptor::NotAProcess => return Err(Errno::EBADF.into()),
                        ProcessDirectoryDescriptor::Unknown(reason) => {
                            tracing::error!(
                                "[detcore, dtid {}] pidfd_send_signal through descriptor {} is not \
                                 supported: {} (https://github.com/rrnewton/hermit/issues/4108)",
                                guest.thread_state().dettid,
                                pidfd,
                                reason,
                            );
                            return self
                                .refuse_unserviceable_operation(
                                    guest,
                                    Sysno::pidfd_send_signal,
                                    Errno::ENOSYS,
                                )
                                .await;
                        }
                    }
                }
            }
        };
        if scope == PidfdSignalScope::ProcessGroup {
            tracing::error!(
                "[detcore, dtid {}] signal {} by pidfd_send_signal with PIDFD_SIGNAL_PROCESS_GROUP \
                 is not supported: Detcore does not model process-group membership, and the \
                 guest's process group is inherited from Hermit's \
                 (https://github.com/rrnewton/hermit/issues/4108)",
                guest.thread_state().dettid,
                signal,
            );
            return self
                .refuse_unserviceable_operation(guest, Sysno::pidfd_send_signal, Errno::ENOSYS)
                .await;
        }
        let process = match named {
            Some(named) => Some(signal_target_process(guest, named).await.unwrap_or(named)),
            None => None,
        };
        // A nested caller's signal to its own PID namespace's init is dropped
        // by the kernel unless the init catches it, SIGKILL always
        // (`Detcore::own_namespace_init_drops`): forward it unchanged, with
        // nothing to retire or record
        // (https://github.com/rrnewton/hermit/issues/4155).
        if guest.thread_state().nested_pid_namespace
            && let Some(process) = process
            && let Ok(Some(local)) = crate::tool_global::local_pid(guest, process).await
            && let crate::scheduler::pid_namespaces::GuestPidAnswer::Task {
                own_init_caught, ..
            } = crate::tool_global::guest_pid(guest, local).await
            && Self::own_namespace_init_drops(own_init_caught, signal)
        {
            return Ok(self.record_or_replay(guest, call).await?);
        }
        // A process Detcore saw exit and that is gone is ESRCH, as Linux
        // answers once it is reaped, whatever the host has reaped by now
        // (https://github.com/rrnewton/hermit/issues/4091).
        if guest.config().sequentialize_threads
            && let Some(process) = process
        {
            let (targets, zombie) = resolve_kill_targets_and_exit(guest, process).await;
            if targets.is_empty() && zombie == Some(false) {
                return self.gone_pidfd_signal_answer(guest, call, signal).await;
            }
        }
        // The thread scope sends to the thread whose id is the pid, like
        // `tgkill(pid, pid)`; the process scope to its process.
        let thread_scope = scope == PidfdSignalScope::Thread;
        let receiver_id = if thread_scope { named } else { process };
        self.record_orphaned_group_signals(guest, signal).await;
        if signal == libc::SIGALRM {
            // `pidfd_open` records the process a pidfd names; a pidfd with no
            // recorded target is refused while any process handles SIGALRM.
            let control = match receiver_id {
                Some(target) => SigalrmControl::SendTo(DetTid::from_raw(target.as_raw())),
                None => SigalrmControl::SendToUnnamed,
            };
            refuse_sigalrm(guest, control).await?;
        }
        // A thread-scoped SIGKILL to a leader that exited while its group
        // lives on changes nothing, as `tgkill`'s does
        // (https://github.com/rrnewton/hermit/issues/4445).
        let exited_leader = signal == libc::SIGKILL
            && guest.config().sequentialize_threads
            && thread_scope
            && match named {
                Some(thread) => {
                    crate::syscalls::signal::sigkill_names_an_exited_leader(
                        guest,
                        DetTid::from_raw(thread.as_raw()),
                    )
                    .await
                }
                None => false,
            };
        if signal == libc::SIGKILL
            && guest.config().sequentialize_threads
            && !exited_leader
            && let (Some(process), Some(receiver)) = (process, receiver_id)
        {
            // A SIGKILL ends the whole process whichever thread it names, and
            // is retired at this turn as `kill`'s and `tgkill`'s are
            // (https://github.com/rrnewton/hermit/issues/3994).
            if self
                .reserve_controlled_self_sigkill_exit(
                    guest,
                    signal,
                    Some(process),
                    thread_scope.then(|| DetTid::from_raw(receiver.as_raw())),
                )
                .await
            {
                return Ok(self.record_or_replay(guest, call).await?);
            }
            if let Some(reservation) =
                reserve_sigkill(guest, DetTid::from_raw(receiver.as_raw())).await
            {
                return self.send_reserved_sigkill(guest, reservation, call).await;
            }
        }
        if thread_scope {
            // A thread-scoped send queues the copy in the named thread's
            // private queue (the Claude re-check of
            // https://github.com/rrnewton/hermit/pull/4131).
            let value = match (named, process) {
                (Some(thread), Some(process)) => {
                    self.send_guest_signal(
                        guest,
                        call,
                        DetTid::from_raw(thread.as_raw()),
                        signal,
                        Some(process),
                        true,
                        DetTid::from_raw(thread.as_raw()),
                    )
                    .await?
                }
                _ => self.record_or_replay(guest, call).await?,
            };
            if exited_leader {
                return Ok(value);
            }
            if let (Some(thread), Some(process)) = (named, process) {
                let thread = DetTid::from_raw(thread.as_raw());
                if signal == libc::SIGKILL {
                    crate::tool_global::notify_signal_pending(
                        guest,
                        thread,
                        SigWrapper(libc::SIGKILL),
                        Some(process),
                        crate::tool_global::SignalScope::Thread,
                    )
                    .await;
                } else {
                    self.notify_cross_task_signal(
                        guest,
                        thread,
                        signal,
                        Some(process),
                        crate::tool_global::SignalScope::Thread,
                    )
                    .await;
                }
            }
            return Ok(value);
        }
        // A process-directed signal. To a process with two or more live threads
        // it is refused by name as `kill` refuses it: Linux's complete_signal
        // picks the receiving thread, which Detcore does not model
        // (https://github.com/rrnewton/hermit/issues/4047). Everything else is
        // forwarded and the kernel answers: signal 0, which only checks that
        // the process exists; a SIGKILL that could not be reserved, which kills
        // the whole group whichever thread it reaches (the scheduler is told
        // below); a process with one live thread, whose receiver the scheduler
        // is then told about, as `kill` does; and a process with none (a
        // zombie, which Linux still signals successfully).
        let mut receiver = None;
        if signal != 0
            && signal != libc::SIGKILL
            && guest.config().sequentialize_threads
            && let Some(target) = process
        {
            match resolve_kill_targets(guest, target).await.as_slice() {
                [] => {}
                [thread] => receiver = Some((*thread, target)),
                _ => {
                    self.process_signal_target(
                        guest,
                        reverie::syscalls::Sysno::pidfd_send_signal,
                        target,
                        signal,
                    )
                    .await?;
                }
            }
        }
        let value = match receiver {
            Some((thread, process)) => {
                // A process-scoped pidfd names the process's leader
                // (`pidfd_send_signal` with `PIDTYPE_TGID`).
                self.send_guest_signal(
                    guest,
                    call,
                    thread,
                    signal,
                    Some(process),
                    false,
                    DetTid::from_raw(process.as_raw()),
                )
                .await?
            }
            None => self.record_or_replay(guest, call).await?,
        };
        if let Some((thread, process)) = receiver {
            // Tell the scheduler, as `kill` does, so a receiver parked in a
            // blocking call (pause(), a futex wait) is woken to take it
            // (https://github.com/rrnewton/hermit/issues/4099).
            self.notify_cross_task_signal(
                guest,
                thread,
                signal,
                Some(process),
                crate::tool_global::SignalScope::Process,
            )
            .await;
        }
        if signal == libc::SIGKILL {
            // A process killed through a pidfd will die; the scheduler must
            // not count a wait for it as part of a deadlock
            // (https://github.com/rrnewton/hermit/issues/3904).
            match process {
                Some(target) => {
                    crate::tool_global::notify_signal_pending(
                        guest,
                        DetTid::from_raw(target.as_raw()),
                        SigWrapper(libc::SIGKILL),
                        Some(target),
                        crate::tool_global::SignalScope::Process,
                    )
                    .await
                }
                None => crate::tool_global::notify_unresolved_sigkill(guest, None).await,
            }
        }
        Ok(value)
    }

    /// The answer to a `pidfd_send_signal` whose target Detcore saw exit and
    /// that is gone, in Linux's order: a non-NULL `info` is copied and its
    /// layout checked against its own `si_signo` (EFAULT, E2BIG), then that
    /// `si_signo` against the signal (EINVAL), then a forged `si_code`
    /// (EPERM), and only then is the task missing (ESRCH). The copy, the layout
    /// check and the `si_code` check are the kernel's: the same `info` goes to
    /// `rt_sigqueueinfo` naming a pid no task can have, which makes those
    /// checks and sends nothing (as
    /// <https://github.com/rrnewton/hermit/issues/4124> does for that call).
    /// `rt_sigqueueinfo` checks the layout against the signal it is given, so
    /// it is given the info's own `si_signo`.
    async fn gone_pidfd_signal_answer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        signal: i32,
    ) -> Result<i64, Error> {
        let info = match call {
            Syscall::Other(_, args) => args.arg2,
            _ => 0,
        };
        let Some(info) = AddrMut::<GuestSiginfo>::from_raw(info) else {
            return Err(Errno::ESRCH.into());
        };
        let signo = guest.memory().read_value(info.cast::<libc::c_int>()).ok();
        let probe = syscalls::RtSigqueueinfo::new()
            .with_tgid(super::signal::NO_SUCH_PID)
            .with_sig(signo.unwrap_or(signal))
            .with_siginfo(Some(info.cast()));
        let checked = guest.inject(probe).await;
        let answer = match checked {
            Err(errno @ (Errno::EPERM | Errno::ESRCH)) => errno,
            Err(errno) => return Err(errno.into()),
            Ok(_) => Errno::ESRCH,
        };
        // The copy succeeded, so the kernel could read `si_signo`; a page
        // only the kernel reaches (`[vvar]`) cannot be decided here.
        let Some(signo) = signo else {
            tracing::error!(
                "[detcore, dtid {}] pidfd_send_signal to a gone process with a siginfo_t at \
                 {:#x} is not supported: Hermit cannot read its si_signo \
                 (https://github.com/rrnewton/hermit/issues/4108)",
                guest.thread_state().dettid,
                info.as_raw(),
            );
            return self
                .refuse_unserviceable_operation(guest, Sysno::pidfd_send_signal, Errno::ENOSYS)
                .await;
        };
        if signo != signal {
            return Err(Errno::EINVAL.into());
        }
        Err(answer.into())
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

        // A nested caller's own getpid and gettid are its namespace's ids,
        // while a pidfd records the task in the container's: compare the
        // caller's own DetPid and DetTid then
        // (https://github.com/rrnewton/hermit/issues/4155).
        let (current_tgid, current_tid) = match guest.thread_state().detpid {
            Some(detpid) if guest.thread_state().nested_pid_namespace => {
                (detpid, guest.thread_state().dettid)
            }
            _ => (
                DetPid::from_raw(guest.inject(syscalls::Getpid::new()).await? as i32),
                DetTid::from_raw(guest.inject(syscalls::Gettid::new()).await? as i32),
            ),
        };
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
        // This process now holds the description too, with any OFD or flock
        // lock it holds (`procfs::locks`).
        self.refresh_description_locks(guest, fd, LockCall::PidfdGetfd)
            .await?;
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
        //
        // Record and replay leave the connection as the guest asked, as `handle_socket` does.
        // Their reads and writes on it run as backgrounded blocking calls, and on a physically
        // nonblocking connection such a call returns EAGAIN to a guest that asked to block
        // (https://github.com/rrnewton/hermit/issues/3882). The rejoin log orders their results.
        // TODO-HUMAN-REVIEW(PR-3908)
        let physically_nonblocking =
            self.cfg.use_nonblocking_sockets() && !self.cfg.recordreplay_modes;
        let call2 = if physically_nonblocking {
            // Let the socket returned from accept4 be physically nonblocking:
            call.with_flags(call.flags() | SockFlag::SOCK_NONBLOCK)
        } else {
            call
        };
        let in_turn = self.accept_in_turn_enabled()
            && guest
                .thread_state()
                .with_detfd(call.sockfd(), |detfd| detfd.is_accept_in_turn_listener())
                .unwrap_or(false);
        let fd = if in_turn {
            // Record and replay run the accept in the caller's turn, so the new descriptor's
            // number is allocated deterministically (https://github.com/rrnewton/hermit/issues/3880).
            self.accept_in_turn(guest, call2).await? as RawFd
        } else {
            // This will do blocking/polling as appropriate based on the fd status:
            self.execute_nonblockable_fd_syscall(guest, call2).await? as RawFd
        };

        self.add_fd(
            guest,
            fd,
            // This will specify whether the socket returned is logically non-blocking:
            oflag_from_sock_bits(call.flags().bits()),
            FdType::Socket,
        )
        .await?;
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.set_accepted_connection())?;

        if physically_nonblocking {
            self.maybe_set_nonblocking_fd(guest, fd);
        }

        Ok(fd as i64)
    }

    /// getdents system call.
    pub async fn handle_getdents<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getdents,
    ) -> Result<i64, Error> {
        let virtualize = guest.config().virtualize_metadata;
        if !virtualize
            && (call.dirent().is_none()
                || !self.is_caller_descriptor_table(guest, call.fd() as i32))
        {
            return Ok(self.record_or_replay(guest, call).await?);
        }
        let buf = call.dirent().ok_or(Errno::EFAULT)?.cast::<u8>();
        let call = GetdentsCall {
            call: Syscall::from(call),
            empty: Syscall::from(call.with_count(0)),
            fd: call.fd() as i32,
            buf,
            capacity: call.count() as usize,
            format: DirentFormat::Legacy,
        };
        if virtualize {
            self.serve_directory_stream(guest, call).await
        } else {
            self.project_one_buffer(guest, call).await
        }
    }

    /// getdents64 system call.
    pub async fn handle_getdents64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getdents64,
    ) -> Result<i64, Error> {
        let virtualize = guest.config().virtualize_metadata;
        if !virtualize
            && (call.dirent().is_none()
                || !self.is_caller_descriptor_table(guest, call.fd() as i32))
        {
            return Ok(self.record_or_replay(guest, call).await?);
        }
        let buf = call.dirent().ok_or(Errno::EFAULT)?.cast::<u8>();
        let call = GetdentsCall {
            call: Syscall::from(call),
            empty: Syscall::from(call.with_count(0)),
            fd: call.fd() as i32,
            buf,
            capacity: call.count() as usize,
            format: DirentFormat::Dirent64,
        };
        if virtualize {
            self.serve_directory_stream(guest, call).await
        } else {
            self.project_one_buffer(guest, call).await
        }
    }

    /// Answer a `getdents` call from the open file's sorted directory stream
    /// (see [`DirectoryStream`]), reading the host directory first if the
    /// stream has no snapshot.
    ///
    /// Sorting each kernel buffer on its own is not enough: a directory larger
    /// than one buffer (about a thousand short names for glibc's 32KiB) would
    /// come back as sorted runs whose boundaries, and the host `d_off` cookies
    /// inside them, depend on the filesystem's on-disk layout.
    async fn serve_directory_stream<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: GetdentsCall<'_>,
    ) -> Result<i64, Error> {
        let negative = i32::try_from(call.capacity).is_err();
        let lock = match guest
            .thread_state()
            .with_detfd(call.fd, |detfd| detfd.directory_lock())
        {
            Ok(lock) => lock,
            // Detcore has no open file description for this descriptor (one
            // received through SCM_RIGHTS, for example), so there is nowhere
            // to keep a stream. Sort the one kernel buffer instead, as before
            // directory streams existed; the kernel reports a descriptor that
            // is not open at all.
            Err(Errno::EBADF) if negative => {
                return self.serve_negative_count(guest, call, None).await;
            }
            Err(Errno::EBADF) => return self.project_one_buffer(guest, call).await,
            Err(errno) => return Err(errno.into()),
        };
        let _serving = lock.lock().await;
        let (host_order, needs_snapshot, position) =
            guest.thread_state().with_detfd(call.fd, |detfd| {
                (
                    detfd.directory_in_host_order(),
                    detfd.directory_needs_snapshot(),
                    detfd.with_directory_stream(|stream| stream.position()).ok(),
                )
            })?;
        if negative {
            let stream = if host_order {
                None
            } else {
                position.map(|position| (position, needs_snapshot))
            };
            return self.serve_negative_count(guest, call, stream).await;
        }
        if host_order {
            return self.project_one_buffer(guest, call).await;
        }
        if !needs_snapshot {
            return self.serve_next_batch(guest, call).await;
        }
        let mark = directory_snapshot_mark(guest).await;
        if let Some((entries, inodes, order)) = self.modeled_directory_snapshot(guest, call).await?
        {
            let mut entries = Some(entries);
            guest.thread_state().with_detfd(call.fd, |detfd| {
                detfd.install_directory_snapshot(
                    entries.take().unwrap_or_default(),
                    mark,
                    inodes.clone(),
                    order,
                )
            })?;
            return self.serve_next_batch(guest, call).await;
        }
        // How to key the entries' numbers is decided with the snapshot, as a
        // procfs directory loses its name once its task exits (see
        // `DirectoryStream`).
        let inodes = self.directory_inodes(guest, call.fd).await?;
        match self.snapshot_directory_privately(guest, call).await {
            Ok(Some(entries)) => {
                let mut entries = Some(entries);
                guest.thread_state().with_detfd(call.fd, |detfd| {
                    detfd.install_directory_snapshot(
                        entries.take().unwrap_or_default(),
                        mark,
                        inodes.clone(),
                        DirectoryOrder::NameBytes,
                    )
                })?;
                self.serve_next_batch(guest, call).await
            }
            Ok(None) => {
                guest
                    .thread_state()
                    .with_detfd(call.fd, |detfd| detfd.use_host_directory_order())?;
                self.project_one_buffer(guest, call).await
            }
            Err(error) => Err(error),
        }
    }

    /// Answer a `getdents` whose count does not fit in an `int`. Linux keeps
    /// the count in one, where it is negative, so no entry fits: the call
    /// fails with `EINVAL`, or returns 0 at the end of the directory, and
    /// moves nothing. It does not look at the buffer, so a count of 0 gets
    /// the same answer for any pointer, and the same `EBADF`, `ENOTDIR` or
    /// `ENOENT`; that is the call issued, since a backend that does not
    /// truncate the guest's count to an `int` would copy host records into
    /// the guest's buffer.
    ///
    /// `stream` is the directory stream's position and whether it needs a
    /// snapshot, or `None` when the open file has no stream (it has not been
    /// read, is read in host order, or is not tracked). Then the guest's
    /// position is the kernel's, so the kernel's answer is Linux's. So it is
    /// for a stream rewound to 0, whose kernel position is the start. A
    /// stream that has a snapshot decides between `EINVAL` and 0 itself. The
    /// kernel position of a stream without a snapshot is moved to the start
    /// first, and then to where the snapshot, if made, puts it. A descriptor
    /// Detcore does not track that shares the open file, and had moved that
    /// position, reads on from there, where on Linux the call moves nothing. A
    /// stream rewound and then seeked past 0 needs to know how many entries
    /// the directory has, so a snapshot is taken (see
    /// [`Self::snapshot_directory_privately`]). If the directory must be
    /// read in host order instead, the kernel's answer from the start is
    /// returned, which is `EINVAL` even at or past the end; if no mapping can
    /// be made for the snapshot, the call fails with `ENOMEM`
    /// (https://github.com/rrnewton/hermit/issues/3723).
    async fn serve_negative_count<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: GetdentsCall<'_>,
        stream: Option<(u64, bool)>,
    ) -> Result<i64, Error> {
        if let Some((_, true)) = stream {
            // Without a snapshot, the kernel position of a stream is the
            // start; a descriptor Detcore does not track may have moved it.
            self.move_directory_kernel_position(guest, call.fd, 0).await;
        }
        let kernel = self.record_or_replay(guest, call.empty).await;
        match kernel {
            Ok(_) | Err(Errno::EINVAL) => {}
            Err(errno) => return Err(errno.into()),
        }
        match stream {
            None | Some((0, true)) => return Ok(kernel.map(|_| 0)?),
            Some((_, true)) => {
                let mark = directory_snapshot_mark(guest).await;
                let modeled = self.modeled_directory_snapshot(guest, call).await?;
                let (snapshot, inodes, order) = match modeled {
                    Some((entries, inodes, order)) => (Ok(Some(entries)), inodes, order),
                    None => {
                        let inodes = self.directory_inodes(guest, call.fd).await?;
                        (
                            self.snapshot_directory_privately(guest, call).await,
                            inodes,
                            DirectoryOrder::NameBytes,
                        )
                    }
                };
                match snapshot {
                    Ok(Some(entries)) => {
                        let mut entries = Some(entries);
                        let target = guest.thread_state().with_detfd(call.fd, |detfd| {
                            detfd.install_directory_snapshot(
                                entries.take().unwrap_or_default(),
                                mark,
                                inodes.clone(),
                                order,
                            );
                            detfd.with_directory_stream(|stream| stream.kernel_target())
                        })??;
                        self.move_directory_kernel_position(guest, call.fd, target)
                            .await;
                    }
                    Ok(None) => return Ok(kernel.map(|_| 0)?),
                    Err(error) => return Err(error),
                }
            }
            Some((_, false)) => {}
        }
        let batch = guest.thread_state().with_detfd(call.fd, |detfd| {
            detfd.with_directory_stream(|stream| stream.next_batch(call.format, 0))
        })??;
        batch?;
        Ok(0)
    }

    /// Return the entries at the stream's position that fit in the guest's
    /// buffer, with determinized inodes, keyed as decided when the snapshot
    /// was taken, and each `d_off` naming the position after its entry,
    /// counting from 1.
    ///
    /// Whatever the outcome, the kernel position then follows the stream (see
    /// [`DirectoryStream::kernel_target`]).
    async fn serve_next_batch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: GetdentsCall<'_>,
    ) -> Result<i64, Error> {
        let (start, batch, mark, inodes) =
            guest.thread_state().with_detfd(call.fd, |detfd| {
                detfd.with_directory_stream(|stream| {
                    (
                        stream.position(),
                        stream.next_batch(call.format, call.capacity),
                        stream.snapshot_mark(),
                        stream.snapshot_inodes(),
                    )
                })
            })??;
        let (passed, copied) = match batch {
            Ok(batch) => {
                // Every snapshot is installed with its keys; a stream without
                // them is keyed now.
                let inodes = match inodes {
                    Some(inodes) => inodes,
                    None => self.directory_inodes(guest, call.fd).await?,
                };
                let mut records = Vec::new();
                let mut names = Vec::with_capacity(batch.len());
                for (index, entry) in batch.iter().enumerate() {
                    let (host, path) = inodes.entry_inode(&entry.name, entry.ino);
                    let files = self.rendered_files(guest);
                    let d_ino = match self
                        .rendered_listed_inode(guest, &inodes, &entry.name, entry.ino, files)
                        .await?
                    {
                        Some(inode) => DetInode::mint(inode),
                        None => determinize_listed_inode(guest, host, path, mark).await.0,
                    };
                    let d_off = i64::try_from(start + index as u64 + 1).unwrap_or(i64::MAX);
                    call.format
                        .encode(entry, d_ino.as_raw(), d_off, &mut records);
                    names.push(entry.name.len());
                }
                copy_records(
                    &mut guest.memory(),
                    call.buf,
                    call.format,
                    &records,
                    &names,
                    start,
                )
            }
            Err(errno) => (0, Err(errno)),
        };
        // With --no-sequentialize-threads another thread can close the
        // descriptor during the copy; like Linux, the call's result stands.
        let target = guest.thread_state().with_detfd(call.fd, |detfd| {
            detfd.with_directory_stream(|stream| {
                stream.advance(passed);
                stream.kernel_target()
            })
        });
        if let Ok(Ok(target)) = target {
            self.move_directory_kernel_position(guest, call.fd, target)
                .await;
        }
        Ok(copied.map(|len| len as i64)?)
    }

    /// The entries of the `/proc` directory open as `fd`, and how to key their
    /// inode numbers, if Detcore lists it from its own model (see
    /// [`crate::procfs::listing`]): the root, the level table's other
    /// directories, a process's or a thread's directory, or a process's
    /// `task`, with processes and threads as the guest numbers them, so that
    /// every backend lists the same.
    ///
    /// The directory is the one the descriptor was opened on: where guest
    /// thread IDs name host tasks (ptrace), the kernel's canonical name for
    /// it (`procfs_entry`), which bound `self` and `thread-self` to the
    /// opener, so an inherited or passed descriptor keeps listing the
    /// opener's; on a backend that serves its own `/proc` (KVM), the guest's
    /// spelling, and only a directory with no process in its path (the root
    /// and the table's system directories), as its process directories cannot
    /// be opened yet (slice B3a-4).
    ///
    /// Each entry is looked up by its spelling below the directory, as the
    /// guest's own `lstat` of it would be: on the kernel's procfs an entry the
    /// lookup does not find (a name the level lists but this kernel lacks,
    /// such as `children` without `CONFIG_PROC_CHILDREN`) is left out, unless
    /// Detcore renders it; on a backend's own /proc (KVM), whose lookups
    /// follow its executor's process table at host-timed moments, a lookup
    /// decides no entry. One the lookup finds is
    /// numbered by the inode it reports, which a stat of it pools under too
    /// (with the entry's name and its task's incarnation), so the two agree. A
    /// file Detcore renders is numbered by its rendered inode
    /// ([`Self::serve_next_batch`]), and an entry the backend refuses to look
    /// up (KVM) by its path's key.
    ///
    /// The kernel's directory is still read once, as for any other directory
    /// ([`Self::snapshot_directory_privately`]), for its `d_off` cookies: each
    /// entry carries the cookie of the host entry of its name, so that the
    /// stream leaves the kernel position where a descriptor Detcore does not
    /// track reads on from (at the end, the kernel's own end). A descriptor
    /// whose position the guest moved before its first read is then read in
    /// host order, as any other is.
    ///
    /// `None`, so that the kernel's listing is served as before: for any other
    /// directory; an `O_PATH` descriptor, which lists nothing (the kernel's
    /// `EBADF`); under record and replay, where the replayer's descriptor
    /// does not name the recorded directory; in a nested PID namespace, whose
    /// numbers are not Detcore's; after a guest `chroot`, where `/proc` need
    /// not be procfs; for a descriptor not on the guest's procfs (a user bind
    /// over part of it); on the in-guest backends whose kernel numbers
    /// processes otherwise than Detcore (DBT,
    /// <https://github.com/rrnewton/hermit/issues/4009>), where a listed guest
    /// pid would open the kernel's process of that number until `/proc/<pid>`
    /// is translated (slice B3a-4); and for a process or thread the scheduler
    /// does not list, which the kernel's directory then answers for.
    async fn modeled_directory_snapshot<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: GetdentsCall<'_>,
    ) -> Result<Option<(Vec<DirEntry>, DirectoryInodes, DirectoryOrder)>, Error> {
        let fd = call.fd;
        use std::os::unix::ffi::OsStrExt;

        use crate::procfs::listing;
        use crate::procfs::listing::ModeledDirectory;
        let state = guest.thread_state();
        if self.cfg.recordreplay_modes
            || state.nested_pid_namespace
            || state.changed_root.load(Ordering::SeqCst)
        {
            return Ok(None);
        }
        let Ok((spelled, status_flags)) =
            state.with_detfd(fd, |detfd| (detfd.path(), detfd.status_flags()))
        else {
            return Ok(None);
        };
        // Only a descriptor the guest opened by a spelling of such a
        // directory is one, which spares every other directory's listing the
        // naming below.
        if status_flags & libc::O_PATH != 0 {
            return Ok(None);
        }
        // Where numbers are translated (DBT), the kernel's name for the
        // descriptor says which directory it is, whatever spelling opened it
        // (`/proc/self/fd/N` of a descriptor of `/proc`).
        let kernel_named = || {
            std::fs::read_link(format!("/proc/thread-self/fd/{fd}"))
                .is_ok_and(|kernel| listing::classify(&kernel, 0, 0).is_some())
        };
        if spelled
            .as_deref()
            .and_then(|spelled| listing::classify(spelled, 0, 0))
            .is_none()
            && !(Self::translates_procfs_numbers(guest)
                && guest.config().backend.tool_shares_guest_descriptor_table
                && kernel_named())
        {
            return Ok(None);
        }
        let state = guest.thread_state();
        let tid = state.dettid.as_raw();
        let names_tasks =
            procfs_inode::guest_tids_name_host_tasks(&self.cfg.backend, state.physical_tid);
        let pid = self.own_detpid(guest).as_raw();
        let cached = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.stat())
            .ok()
            .flatten();
        let stat = match cached {
            Some(stat) => stat,
            None => DetStat::from(self.inject_fstat(guest, fd).await?),
        };
        if stat.mode & libc::S_IFMT != libc::S_IFDIR {
            return Ok(None);
        }
        let backend_procfs =
            procfs_inode::is_backend_procfs_device(&self.cfg.backend, stat.dev, stat.blksize);
        // Where the kernel numbers processes otherwise than Detcore (DBT),
        // the directory is the one the guest's spelling names, which reached
        // the kernel's directory of the same process (`procfs_numbers`).
        let translates = Self::translates_procfs_numbers(guest)
            && rendered_procfs::proc_device() == Some(stat.dev);
        let path = if names_tasks && rendered_procfs::proc_device() == Some(stat.dev) {
            match Self::procfs_entry(guest, &stat, StatTarget::Descriptor(fd)).await {
                ProcfsEntry::At(within) => {
                    Path::new("/proc").join(within.strip_prefix("/").unwrap_or(&within))
                }
                ProcfsEntry::NotProcfs | ProcfsEntry::Unnamed | ProcfsEntry::Unknown => {
                    return Ok(None);
                }
            }
        } else if translates {
            match Self::guest_procfs_spelling(guest, fd).await {
                Some(path) => path,
                None => return Ok(None),
            }
        } else if backend_procfs && let Some(spelled) = spelled {
            spelled
        } else {
            return Ok(None);
        };
        let Some(directory) = listing::classify(&path, pid, tid) else {
            return Ok(None);
        };
        if backend_procfs && !matches!(directory, ModeledDirectory::System(_)) {
            return Ok(None);
        }
        let process = match directory {
            ModeledDirectory::System(_) => None,
            ModeledDirectory::Process(process)
            | ModeledDirectory::Thread(process, _)
            | ModeledDirectory::Tasks(process) => Some(DetPid::from_raw(process)),
        };
        let facts = crate::tool_global::procfs_listing(guest, process).await;
        let listed = match directory {
            ModeledDirectory::System(_) => true,
            // The namespace's init is Hermit's own process unless a guest
            // holds pid 1; its threads are the kernel's to list.
            ModeledDirectory::Process(process) => {
                process == 1 || facts.processes.contains(&process)
            }
            ModeledDirectory::Thread(_, thread) => facts.threads.contains(&thread),
            ModeledDirectory::Tasks(_) => !facts.threads.is_empty(),
        };
        if !listed {
            return Ok(None);
        }
        let Some(names) = listing::entries(&directory, self.cfg.proc_level, &facts) else {
            return Ok(None);
        };
        // The host's cookies, where the kernel lists the directory. A
        // backend's own /proc (KVM) lists nothing of it and may not answer
        // the guest's form of the call (legacy `getdents`), so there only a
        // position the guest moved before its first read is the kernel's to
        // keep, as for any other directory.
        let host = if backend_procfs {
            let known = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.has_directory_stream())?;
            let position = syscalls::Lseek::new()
                .with_fd(fd)
                .with_offset(0)
                .with_whence(Whence::SEEK_CUR);
            if !known && self.record_or_replay(guest, position).await != Ok(0) {
                return Ok(None);
            }
            Vec::new()
        } else {
            let Some(host) = self.snapshot_directory_privately(guest, call).await? else {
                return Ok(None);
            };
            host
        };
        // A process or thread number the kernel lists is the kernel's (DBT):
        // its cookie is the cookie of the guest's number for it.
        let host_numbers = if translates {
            let held = host
                .iter()
                .filter(|entry| {
                    crate::procfs::ids::names_an_id(&path, std::ffi::OsStr::from_bytes(&entry.name))
                })
                .filter_map(|entry| std::str::from_utf8(&entry.name).ok()?.parse::<i32>().ok())
                .collect::<Vec<_>>();
            Self::guest_procfs_numbers(guest, &held)
                .await
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        // A kernel number that is no guest's has no cookie to give: its name
        // may be another guest number's.
        let host_cookies: std::collections::HashMap<Vec<u8>, i64> = host
            .iter()
            .filter_map(|entry| {
                if !translates
                    || !crate::procfs::ids::names_an_id(
                        &path,
                        std::ffi::OsStr::from_bytes(&entry.name),
                    )
                {
                    return Some((entry.name.clone(), entry.off));
                }
                let kernel = std::str::from_utf8(&entry.name).ok()?.parse::<i32>().ok()?;
                let guest = host_numbers.get(&kernel)?;
                Some((guest.to_string().into_bytes(), entry.off))
            })
            .collect();
        let host_cookie = |name: &[u8]| host_cookies.get(name).copied().unwrap_or(0);
        let session_files = self.rendered_files(guest);
        let host_end = host.last().map_or(0, |entry| entry.off);
        // Looked up from the guest's own descriptor where it names the
        // kernel's directory, and by the guest's spelling on KVM, whose own
        // /proc looks up only absolute paths, and on DBT, whose lookup
        // translates the guest's numbers to the kernel's as the guest's own
        // lookup of each would be (`inject_statat`, `procfs_numbers`).
        let (lookup_dir, spellings): (RawFd, Vec<PathBuf>) = if backend_procfs || translates {
            let spellings = names
                .iter()
                .map(|(name, _)| path.join(std::ffi::OsStr::from_bytes(name)))
                .collect();
            (libc::AT_FDCWD, spellings)
        } else {
            let spellings = names
                .iter()
                .map(|(name, _)| PathBuf::from(std::ffi::OsStr::from_bytes(name)))
                .collect();
            (fd, spellings)
        };
        let looked_up = self.looked_up_inodes(guest, lookup_dir, &spellings).await?;
        let within = listing::within(&directory);
        let entries = names
            .into_iter()
            .zip(looked_up)
            .filter_map(|((name, ty), looked_up)| {
                let entry = match name.as_slice() {
                    b"." => within.clone(),
                    b".." => within.parent().unwrap_or(&within).to_path_buf(),
                    name => within.join(std::ffi::OsStr::from_bytes(name)),
                };
                // A file Detcore renders exists whatever the kernel has.
                let rendered = crate::procfs::ProcfsFile::from_path(
                    &Path::new("/proc").join(entry.strip_prefix("/").unwrap_or(&entry)),
                )
                .is_some_and(|file| file.rendered_path(session_files).is_some());
                // On a backend's own /proc (KVM), what a lookup finds is the
                // backend's, at a moment the host decides (a process's
                // directory goes when its executor learns of the exit), so
                // there it numbers an entry but decides none.
                let ino = match looked_up {
                    Ok(ino) => ino,
                    Err(Errno::ENOENT)
                        if name != b"." && name != b".." && !rendered && !backend_procfs =>
                    {
                        return None;
                    }
                    Err(_) => procfs_inode::path_inode(stat.dev, &entry).ino,
                };
                Some(DirEntry {
                    ino,
                    off: host_cookie(&name),
                    ty,
                    name,
                })
            })
            .collect::<Vec<_>>();
        // In host order, which `DirectoryStream::install` takes them in, and
        // ending where the host's listing ends, so the stream's end is the
        // kernel's.
        let mut entries = entries;
        entries.sort_by_key(|entry| entry.off);
        if let Some(last) = entries.last_mut() {
            last.off = last.off.max(host_end);
        }
        let inodes = DirectoryInodes::new(stat.dev, Some(ProcfsDirectory::new(within)));
        let order = match &directory {
            ModeledDirectory::System(names) if names.is_empty() => DirectoryOrder::ProcfsIds,
            ModeledDirectory::Tasks(_) => DirectoryOrder::ProcfsIds,
            _ => DirectoryOrder::NameBytes,
        };
        Ok(Some((entries, inodes, order)))
    }

    /// The inode number a lookup of each of `paths` from the directory open as
    /// `dir` (or absolute, with `AT_FDCWD`), without following a final link,
    /// gives in the guest, as the guest's own `fstatat` of it would, or the
    /// lookup's error. The paths are staged in an
    /// anonymous mapping injected for the purpose and unmapped before the guest
    /// runs again, as a directory snapshot's reads are (see
    /// [`Self::snapshot_directory_privately`]); without one, every lookup is
    /// `ENOMEM`. A mapping that cannot be unmapped would be visible to the
    /// guest, so that is an error.
    async fn looked_up_inodes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        dir: RawFd,
        paths: &[PathBuf],
    ) -> Result<Vec<Result<u64, Errno>>, Error> {
        use std::os::unix::ffi::OsStrExt;

        let mapped = guest
            .inject_with_retry(Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(DIRECTORY_PAGE)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await;
        let Some(page) = mapped
            .ok()
            .and_then(|addr| usize::try_from(addr).ok())
            .and_then(AddrMut::<u8>::from_raw)
        else {
            return Ok(vec![Err(Errno::ENOMEM); paths.len()]);
        };
        let mut inodes = Vec::with_capacity(paths.len());
        for path in paths {
            let mut bytes = path.as_os_str().as_bytes().to_vec();
            bytes.push(0);
            let staged =
                bytes.len() <= DIRECTORY_PAGE && guest.memory().write_exact(page, &bytes).is_ok();
            let staged_path = PathPtr::from_ptr(page.as_raw() as *const libc::c_char);
            let inode = match (staged, staged_path) {
                (true, Some(staged_path)) => Self::inject_statat(
                    guest,
                    dir,
                    staged_path,
                    [Some(staged_path), None],
                    AtFlags::AT_SYMLINK_NOFOLLOW,
                )
                .await
                .map(|stat| stat.st_ino),
                _ => Err(Errno::ENAMETOOLONG),
            };
            inodes.push(inode);
        }
        guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Some(Addr::from(page).cast()))
                    .with_len(DIRECTORY_PAGE),
            ))
            .await?;
        Ok(inodes)
    }

    /// How to key the inode numbers of the entries of the directory open as
    /// `fd`. Every entry is on the directory's host device (a mount point's
    /// entry names the directory beneath the mount, on the same device), and
    /// a procfs directory's entries also name their paths beneath its
    /// canonical path (see [`procfs_inode`]). The directory's stat is the
    /// cached one when Detcore tracks `fd`, and an injected `fstat` otherwise.
    async fn directory_inodes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<DirectoryInodes, Errno> {
        let cached = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.stat())
            .ok()
            .flatten();
        let stat = match cached {
            Some(stat) => stat,
            None => DetStat::from(self.inject_fstat(guest, fd).await?),
        };
        let procfs = match Self::procfs_entry(guest, &stat, StatTarget::Descriptor(fd)).await {
            ProcfsEntry::At(within) => Some(ProcfsDirectory::new(within)),
            ProcfsEntry::NotProcfs | ProcfsEntry::Unnamed | ProcfsEntry::Unknown => None,
        };
        Ok(DirectoryInodes::new(stat.dev, procfs))
    }

    /// Move the kernel position of the open file behind `fd` to `target`, a
    /// [`DirectoryStream::kernel_target`], so that a descriptor Detcore does
    /// not track that aliases it reads every entry the stream has not
    /// returned. Without this, the snapshot leaves the kernel at the end of
    /// the directory, and such a descriptor would read nothing.
    ///
    /// The guest's own call has already been decided, so a failed seek is not
    /// reported to it; the next move of the stream seeks again.
    async fn move_directory_kernel_position<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        target: i64,
    ) {
        let seek = syscalls::Lseek::new()
            .with_fd(fd)
            .with_offset(target)
            .with_whence(Whence::SEEK_SET);
        let _ = self.record_or_replay(guest, seek).await;
    }

    /// Drop from `entries`, read from the directory open as `fd`, the CPUs and
    /// nodes that Hermit's single virtual CPU and node leave out (see
    /// [`is_absent_cpu_entry`]), so that a guest counting
    /// `/sys/devices/system/cpu/cpuN` finds one CPU, as `online` and
    /// `sched_getaffinity` report
    /// (<https://github.com/rrnewton/hermit/issues/4160>); and the `/proc`
    /// names outside the run's `--proc` level, which lookups refuse (see
    /// [`levels::absent_entry`]).
    ///
    /// The directory is named by the kernel's path for the descriptor, so a
    /// relative or symlinked spelling is recognized too. Not under record and
    /// replay, where the replayer's descriptor does not name the recorded
    /// directory, so the two runs would not drop alike; Detcore names no
    /// procfs entry there either (see [`Self::procfs_entry`]).
    fn drop_absent_entries<G: Guest<Self>>(
        &self,
        guest: &G,
        fd: RawFd,
        entries: &mut Vec<DirEntry>,
    ) {
        if self.cfg.recordreplay_modes {
            return;
        }
        let link = format!("{}/fd/{fd}", self.caller_proc_directory(guest));
        // Names are dropped as outside the level only from a directory the
        // tool sees is on procfs: not from a user bind over part of `/proc`,
        // and not where the tool cannot tell (its `/proc` entry for the
        // caller is not the caller's where the guest's threads are no host
        // tasks, KVM, or is unreadable after a `chroot`).
        let tool_sees_caller =
            crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend);
        let directory_metadata = tool_sees_caller
            .then(|| std::fs::metadata(&link).ok())
            .flatten();
        let on_procfs = match (&directory_metadata, rendered_procfs::proc_device()) {
            (Some(metadata), Some(device)) => metadata.dev() == device,
            _ => false,
        };
        if self.is_caller_descriptor_table(guest, fd) {
            entries.retain(|entry| {
                !runtime_owned_descriptor_entry(&entry.name, |fd| guest.is_runtime_owned_fd(fd))
            });
        }
        // The kernel's path for the descriptor; or, where the tool cannot
        // read it (after a `chroot` into a root without `/proc`, where the
        // tool runs inside the guest), the path the guest opened it by.
        let directory = std::fs::read_link(&link)
            .ok()
            .or_else(|| {
                guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.path())
                    .ok()
                    .flatten()
            })
            .filter(|path| path.is_absolute());
        // After a `chroot` into another root, the kernel names the directory
        // in the tool's view, below that root.
        let directory = match (directory, self.guest_root_in_view(guest)) {
            (Some(directory), Some(Some(root))) if !directory.starts_with("/proc") => directory
                .strip_prefix(&root)
                .ok()
                .map(|below| Path::new("/").join(below))
                .or(Some(directory)),
            (directory, _) => directory,
        };
        if let Some(directory) = directory {
            let level = self.cfg.proc_level;
            entries.retain(|entry| {
                !is_absent_cpu_entry(&directory, &entry.name)
                    && !(on_procfs && levels::absent_entry(&directory, &entry.name, level))
            });
        }
    }

    /// Whether the opened procfs object names the caller's own descriptor
    /// table. Authenticate kernel object identity, not the guest spelling:
    /// an inherited directory may belong to a different process. Process and
    /// thread aliases have distinct inodes; the process alias requires the
    /// existing shared-table proof. Independent of metadata virtualization.
    fn is_caller_descriptor_table<G: Guest<Self>>(&self, guest: &G, fd: RawFd) -> bool {
        if self.cfg.recordreplay_modes
            || !crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend)
        {
            return false;
        }
        let link = format!("{}/fd/{fd}", self.caller_proc_directory(guest));
        let directory_metadata = std::fs::metadata(link).ok();
        let on_procfs = match (&directory_metadata, rendered_procfs::proc_device()) {
            (Some(metadata), Some(device)) => metadata.dev() == device,
            _ => false,
        };
        let state = guest.thread_state();
        let holds_process_table =
            state.dettid == self.own_detpid(guest) || state.holds_process_files();
        let process = if self.cfg.backend.tool_shares_guest_descriptor_table {
            String::from("/proc/self")
        } else {
            format!("/proc/{}", guest.pid().as_raw())
        };
        on_procfs
            && directory_metadata.as_ref().is_some_and(|metadata| {
                is_caller_descriptor_directory(
                    metadata,
                    &self.caller_proc_directory(guest),
                    holds_process_table.then_some(process.as_str()),
                )
            })
    }

    /// Answer `getdents` without a directory stream by issuing the guest's own
    /// call. With metadata virtualization, sort that kernel buffer and
    /// determinize its inodes. Without it, only omit runtime-owned entries
    /// from an authenticated caller descriptor table; keep host order and
    /// inodes. The host's `d_off` cookies are kept, so the kernel can
    /// still interpret them. A directory larger than one buffer comes back as
    /// sorted runs whose boundaries depend on the host.
    ///
    /// A buffer whose every entry is dropped (see
    /// [`Self::drop_absent_entries`]) is not returned empty, which would
    /// end the directory: the call is issued again from where the kernel
    /// left off. The bytes the kernel wrote past the entries returned, by this
    /// call or one it reissued, are zeroed, as they may name the entries
    /// dropped; so is a buffer every dropped entry was written to when the
    /// directory then ends.
    async fn project_one_buffer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: GetdentsCall<'_>,
    ) -> Result<i64, Error> {
        // The longest any call wrote: a reissued call can write less than the
        // one before it, whose dropped names lie past it.
        let virtualize = guest.config().virtualize_metadata;
        let mut written = 0;
        let mut dropped = false;
        let (len, mut entries) = loop {
            let len = match self.record_or_replay(guest, call.call).await {
                Ok(len) => len as usize,
                Err(error) => {
                    if written > 0 {
                        guest.memory().write_exact(call.buf, &vec![0; written])?;
                    }
                    return Err(error.into());
                }
            };
            if len == 0 {
                if written > 0 {
                    guest.memory().write_exact(call.buf, &vec![0; written])?;
                }
                return Ok(0);
            }
            written = written.max(len);
            let mut entries = match read_records(&guest.memory(), call.buf, len, call.format) {
                Ok(entries) => entries,
                Err(error) => {
                    guest.memory().write_exact(call.buf, &vec![0; written])?;
                    return Err(error.into());
                }
            };
            if virtualize {
                self.drop_absent_entries(guest, call.fd, &mut entries);
            } else {
                let original = entries.len();
                entries.retain(|entry| {
                    !runtime_owned_descriptor_entry(&entry.name, |fd| guest.is_runtime_owned_fd(fd))
                });
                if entries.len() == original && !dropped {
                    return Ok(len as i64);
                }
                dropped = true;
            }
            if !entries.is_empty() {
                break (len, entries);
            }
        };
        let inodes = if virtualize {
            sort_dir_entries(&mut entries);
            Some(self.directory_inodes(guest, call.fd).await?)
        } else {
            None
        };
        let mut records = Vec::with_capacity(len);
        for entry in &entries {
            let Some(inodes) = &inodes else {
                call.format
                    .encode(entry, entry.ino, entry.off, &mut records);
                continue;
            };
            let (host, path) = inodes.entry_inode(&entry.name, entry.ino);
            let files = self.rendered_files(guest);
            let d_ino = match self
                .rendered_listed_inode(guest, inodes, &entry.name, entry.ino, files)
                .await?
            {
                Some(inode) => DetInode::mint(inode),
                None => determinize_named_inode(guest, host, path).await.0,
            };
            call.format
                .encode(entry, d_ino.as_raw(), entry.off, &mut records);
        }
        let returned = records.len();
        // Zero what the kernel wrote past the entries returned.
        records.resize(returned.max(written), 0);
        if guest.memory().write_exact(call.buf, &records).is_err() {
            // The padding after the last name may not be writable, as Linux
            // does not write it.
            let written = entries.last().map_or(0, |last| {
                returned - call.format.record_len(last.name.len())
                    + call.format.written_len(last.name.len())
            });
            guest.memory().write_exact(call.buf, &records[..written])?;
        }
        Ok(returned as i64)
    }

    /// Read the whole host directory behind `call.fd` into a private mapping
    /// (see [`Self::snapshot_directory`]), and return its entries in host
    /// order, less the CPUs and nodes Hermit's single virtual CPU and node
    /// leave out and the `/proc` names outside the run's level (see
    /// [`Self::drop_absent_entries`]). Every directory
    /// snapshot a stream stores comes from here, so none holds them.
    ///
    /// Linux writes into guest memory only the records a call returns and the
    /// fields it stored of one it could not finish, so the reads do not go to
    /// the guest's buffer, which may be too small
    /// for any entry, unreadable, or partly unmapped, nor to its stack. They
    /// go to an anonymous mapping injected for the purpose and unmapped
    /// before the guest runs again, as the atomic `writev` does for its
    /// iovecs. The mapping is fresh, so the reads leave nothing that the
    /// guest could see in memory. On the kvm backend, whose guest allocator
    /// fills the lowest hole below a cursor first and moves the cursor past
    /// each mapping it makes, the mapping moves that cursor, so a later
    /// mapping that fits no hole below it lands elsewhere than on Linux. That
    /// follows from the guest's own calls, so it is the same on every run.
    ///
    /// If a mapping of [`DIRECTORY_DRAIN_COUNT`] bytes cannot be made, one
    /// page is used, which still holds any entry. If that cannot be made
    /// either (the guest's address space is at its limit), the call fails
    /// with `ENOMEM` before anything is read, and the stream stays without a
    /// snapshot, so a later call tries again. Linux's `getdents` does not
    /// fail so; serving the directory in host order would make the order
    /// depend on the host. This divergence is tracked in
    /// https://github.com/rrnewton/hermit/issues/3723.
    async fn snapshot_directory_privately<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: GetdentsCall<'_>,
    ) -> Result<Option<Vec<DirEntry>>, Error> {
        let mut mapping = None;
        for len in [DIRECTORY_DRAIN_COUNT as usize, DIRECTORY_PAGE] {
            let mapped = guest
                .inject_with_retry(Syscall::Mmap(
                    syscalls::Mmap::new()
                        .with_addr(None)
                        .with_len(len)
                        .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                        .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                        .with_fd(-1)
                        .with_offset(0),
                ))
                .await;
            if let Some(scratch) = mapped
                .ok()
                .and_then(|addr| usize::try_from(addr).ok())
                .and_then(AddrMut::<u8>::from_raw)
            {
                mapping = Some((scratch, len));
                break;
            }
        }
        let Some((scratch, len)) = mapping else {
            return Err(Errno::ENOMEM.into());
        };
        let snapshot = self
            .snapshot_directory(guest, call.into_scratch(scratch, len as u32))
            .await;
        let unmapped = guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Some(Addr::from(scratch).cast()))
                    .with_len(len),
            ))
            .await;
        // A mapping left behind would be visible to the guest.
        unmapped?;
        let mut snapshot = snapshot?;
        if let Some(entries) = &mut snapshot {
            self.drop_absent_entries(guest, call.fd, entries);
        }
        Ok(snapshot)
    }

    /// Read the whole host directory by issuing `call.call` until it returns
    /// 0, and return its entries in host order. `call` reads into a private
    /// mapping that holds any entry.
    ///
    /// Every step goes through `record_or_replay`, so a replay reads the same
    /// entries from the log. The reads must start at the beginning of the
    /// directory: a known directory is rewound there, and before the first
    /// snapshot the kernel position must already be 0.
    ///
    /// The first read fails as the guest's call would at any position: the
    /// descriptor is not a directory, or the directory has been removed.
    ///
    /// Returns `None` when this open file must be read in host order instead,
    /// with the kernel position where the guest left it. Either the position
    /// was moved before the first `getdents`, to a host cookie that no entry
    /// index stands for, or a read after the first failed, so the directory
    /// cannot be read to its end. A regular file, pipe or `O_PATH` descriptor
    /// whose position is not 0 also lands here, and reading it in host order
    /// reports the kernel's error without moving its offset.
    async fn snapshot_directory<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: GetdentsCall<'_>,
    ) -> Result<Option<Vec<DirEntry>>, Error> {
        let known_directory = guest
            .thread_state()
            .with_detfd(call.fd, |detfd| detfd.has_directory_stream())?;
        let lseek = |offset, whence| {
            syscalls::Lseek::new()
                .with_fd(call.fd)
                .with_offset(offset)
                .with_whence(whence)
        };
        if known_directory {
            self.record_or_replay(guest, lseek(0, Whence::SEEK_SET))
                .await?;
        } else if self
            .record_or_replay(guest, lseek(0, Whence::SEEK_CUR))
            .await
            != Ok(0)
        {
            return Ok(None);
        }

        let mut entries = Vec::new();
        let mut read_any = false;
        let error: Error = loop {
            let len = match self.record_or_replay(guest, call.call).await {
                Ok(0) => return Ok(Some(entries)),
                Ok(len) => len as usize,
                Err(error) => break error.into(),
            };
            read_any = true;
            match read_records(&guest.memory(), call.buf, len, call.format) {
                Ok(parsed) => entries.extend(parsed),
                Err(error) => break error.into(),
            }
        };
        if !read_any {
            return Err(error);
        }
        // The earlier reads moved the position, which the guest's call must
        // find where the guest left it: at the start.
        self.record_or_replay(guest, lseek(0, Whence::SEEK_SET))
            .await?;
        Ok(None)
    }
}

/// The size of the private mapping that a snapshot reads the host directory
/// into, and so the most bytes one read of it asks for. It holds any entry.
const DIRECTORY_DRAIN_COUNT: u32 = 64 * 1024;

/// The granularity at which the guest's buffer can stop being writable.
const DIRECTORY_PAGE: usize = 4096;

/// The most pages one vectored copy names: Linux's `IOV_MAX`.
const PAGES_PER_COPY: usize = 1024;

/// The pieces of the `len` bytes at guest address `addr`, split where they
/// cross a page: the offset and length of each.
fn guest_pages(addr: usize, len: usize) -> Vec<(usize, usize)> {
    let mut pieces = Vec::with_capacity(len / DIRECTORY_PAGE + 2);
    let mut at = 0;
    while at < len {
        let piece = (DIRECTORY_PAGE - (addr + at) % DIRECTORY_PAGE).min(len - at);
        pieces.push((at, piece));
        at += piece;
    }
    pieces
}

/// Read and decode the `len` bytes of records a `getdents` returned in the
/// guest's buffer. Linux does not write the padding after the last record's
/// name, which can lie in a page the guest cannot read.
fn read_records(
    memory: &impl MemoryAccess,
    buf: AddrMut<u8>,
    len: usize,
    format: DirentFormat,
) -> Result<Vec<DirEntry>, Errno> {
    let mut bytes = vec![0; len];
    let readable = match memory.read_exact(buf, &mut bytes) {
        Ok(()) => len,
        Err(_) => read_guest_prefix(memory, buf, &mut bytes)?,
    };
    format.parse_written(&mut bytes, readable)
}

/// Read the start of the guest's buffer into `bytes`, up to the first page the
/// guest cannot read, and return how many bytes were read. Only a fault
/// (`EFAULT`) ends the copy short; any other error of the copy, such as
/// `ESRCH` for a thread that is gone or `EIO` from the backend, is returned.
///
/// This and [`write_guest_pieces`] use only vectored copies, one page per
/// piece, which respect the guest's page protection. Reverie's `read` and
/// `write` do not always: on the ptrace backend, a copy of at most 8 bytes goes
/// through `PTRACE_PEEKDATA` or `PTRACE_POKEDATA`, which reach a page the guest
/// has made inaccessible.
pub(crate) fn read_guest_prefix(
    memory: &impl MemoryAccess,
    buf: AddrMut<u8>,
    bytes: &mut [u8],
) -> Result<usize, Errno> {
    let mut done = 0;
    for pages in guest_pages(buf.as_raw(), bytes.len()).chunks(PAGES_PER_COPY) {
        let from = pages[0].0;
        let len: usize = pages.iter().map(|&(_, len)| len).sum();
        let remote: Vec<AddrSlice<u8>> = pages
            .iter()
            .map(|&(at, len)| unsafe { AddrSlice::from_raw_parts(buf.add(at).into(), len) })
            .collect();
        let remote: Vec<std::io::IoSlice> = remote
            .iter()
            .map(|piece| unsafe { piece.as_ioslice() })
            .collect();
        let mut local = [std::io::IoSliceMut::new(&mut bytes[from..from + len])];
        let copied = match memory.read_vectored(&remote, &mut local) {
            Err(Errno::EFAULT) => 0,
            copied => copied?,
        };
        done += copied;
        if copied < len {
            break;
        }
    }
    Ok(done)
}

/// Write each of `pieces`, an offset in the guest's buffer and the bytes to
/// store there, within one page, in the order given, up to the first the
/// guest cannot write. Return how many bytes of the pieces were written,
/// counted in that order. As in [`read_guest_prefix`], only a fault ends the
/// copy short, and any other error is returned.
fn write_guest_pieces(
    memory: &mut impl MemoryAccess,
    buf: AddrMut<u8>,
    pieces: &[(usize, &[u8])],
) -> Result<usize, Errno> {
    let mut done = 0;
    for pieces in pieces.chunks(PAGES_PER_COPY) {
        let len: usize = pieces.iter().map(|(_, bytes)| bytes.len()).sum();
        let mut remote: Vec<AddrSliceMut<u8>> = pieces
            .iter()
            .map(|&(at, bytes)| unsafe { AddrSliceMut::from_raw_parts(buf.add(at), bytes.len()) })
            .collect();
        let mut remote: Vec<std::io::IoSliceMut> = remote
            .iter_mut()
            .map(|piece| unsafe { piece.as_ioslice_mut() })
            .collect();
        let local: Vec<std::io::IoSlice> = pieces
            .iter()
            .map(|&(_, bytes)| std::io::IoSlice::new(bytes))
            .collect();
        let copied = match memory.write_vectored(&local, &mut remote) {
            Err(Errno::EFAULT) => 0,
            copied => copied?,
        };
        done += copied;
        if copied < len {
            break;
        }
    }
    Ok(done)
}

/// One store that Linux makes into the guest's buffer to copy directory
/// records: where in the buffer, the bytes stored (a range of the records,
/// or of the first record's placeholder `d_off` after them), and whether the
/// CPU makes it as one store, which writes nothing if any byte of it faults.
struct DirentStore {
    at: usize,
    from: std::ops::Range<usize>,
    single: bool,
}

/// The stores Linux makes, in its order, to copy records whose names are
/// `names` bytes long, and for each record, how many of the stores precede
/// the end of its `filldir`. `placeholder` is where the first record's
/// placeholder `d_off` follows the records.
///
/// For each record, `filldir` (fs/readdir.c) first stores the `d_off` of the
/// record before it, which is its own entry's position. The first record of
/// a call has none before it, so its own `d_off` gets that position, to be
/// overwritten. Then come `d_ino`, `d_reclen`, `d_type`, the NUL after the
/// name, and the name. Once no more records are copied, the call stores the
/// last one's `d_off`. No padding is written.
///
/// Linux copies a name in words from its start, after the NUL that follows
/// it. A name spans at most two pages, and the NUL's page could be written,
/// so if a word of the name faults, the first does, and none of the name is
/// written. So it is here, where the name is written page by page.
fn dirent_stores(
    format: DirentFormat,
    names: &[usize],
    placeholder: usize,
) -> (Vec<DirentStore>, Vec<usize>) {
    let field = |at: usize, len: usize| DirentStore {
        at,
        from: at..at + len,
        single: true,
    };
    let mut stores = Vec::with_capacity(names.len() * 6 + 1);
    let mut filled = Vec::with_capacity(names.len());
    let mut previous = None;
    let mut at = 0;
    for &name_len in names {
        let reclen = format.record_len(name_len);
        let name = at + format.name_offset();
        stores.push(match previous {
            Some(previous) => field(previous + 8, 8),
            None => DirentStore {
                at: at + 8,
                from: placeholder..placeholder + 8,
                single: true,
            },
        });
        stores.push(field(at, 8));
        stores.push(field(at + 16, 2));
        stores.push(field(at + format.type_offset(reclen), 1));
        stores.push(field(name + name_len, 1));
        stores.push(DirentStore {
            at: name,
            from: name..name + name_len,
            single: false,
        });
        filled.push(stores.len());
        previous = Some(at);
        at += reclen;
    }
    if let Some(previous) = previous {
        stores.push(field(previous + 8, 8));
    }
    (stores, filled)
}

/// Copy directory records into the guest's buffer as Linux does: store by
/// store (see [`dirent_stores`]), up to the first store the guest cannot
/// write. `records` holds the records of entries whose names are `names`
/// bytes long, the first at position `start`.
///
/// Return how many entries the call passes, and its result: the bytes of the
/// records whose `filldir` completed. It is `EFAULT` if none did, or if the
/// fault hit a `d_off`, which the call's last store would then write again.
/// A copy that fails with any error other than a fault passes no entry and
/// returns that error, as the call did before directory streams.
///
/// Each store within one page is written whole or not at all, as Linux's
/// are. A single store that crosses into another page, a field of a buffer
/// not aligned to 8 bytes, the CPU writes whole or not at all, so its two
/// parts are written one after the other and the first is put back if the
/// second cannot be written. What is put back is read just before the first
/// part is written, after every store before it, which may have changed those
/// bytes through another mapping of the same memory. The part written first
/// is the one whose bytes Detcore can read to put back: the part in the
/// second page, unless only the part in the first page can be read. Where
/// neither can be read with a vectored copy, the part in the second page is
/// written first, so that a write-only buffer ending at a page the guest
/// cannot touch is left as Linux leaves it, and its bytes are read to put
/// back with Reverie's `read`, which for at most 8 bytes is `PTRACE_PEEKDATA`
/// on the ptrace backend and reads a write-only page. While no other thread
/// writes the buffer, only a store from a page the guest can neither read nor
/// write into a write-only page that this read cannot reach either leaves
/// bytes Linux does not write: up to 7, in the write-only page.
fn copy_records(
    memory: &mut impl MemoryAccess,
    buf: AddrMut<u8>,
    format: DirentFormat,
    records: &[u8],
    names: &[usize],
    start: u64,
) -> (usize, Result<usize, Errno>) {
    if names.is_empty() {
        return (0, Ok(0));
    }
    let mut source = records.to_vec();
    let placeholder = source.len();
    source.extend_from_slice(&i64::try_from(start).unwrap_or(i64::MAX).to_ne_bytes());
    let (stores, filled) = dirent_stores(format, names, placeholder);
    // Each piece, within one page: where in the buffer and its bytes, and,
    // in `owner`, its store.
    let mut pieces: Vec<(usize, &[u8])> = Vec::with_capacity(stores.len());
    let mut owner = Vec::with_capacity(stores.len());
    // For each single store that crosses pages: its part written first, as an
    // index into `pieces`, and whether the guest can read that part's bytes.
    let mut crossing = Vec::new();
    for (index, store) in stores.iter().enumerate() {
        let mut split = guest_pages(buf.as_raw() + store.at, store.from.len());
        if store.single && split.len() > 1 {
            let readable: Result<Vec<bool>, Errno> = split
                .iter()
                .map(|&(at, len)| {
                    let mut bytes = vec![0; len];
                    let addr = unsafe { buf.add(store.at + at) };
                    Ok(read_guest_prefix(memory, addr, &mut bytes)? == len)
                })
                .collect();
            let mut readable = match readable {
                Ok(readable) => readable,
                Err(errno) => return (0, Err(errno)),
            };
            if readable[1] || !readable[0] {
                split.reverse();
                readable.reverse();
            }
            crossing.push((pieces.len(), readable[0]));
        }
        for (at, len) in split {
            let from = store.from.start + at;
            pieces.push((store.at + at, &source[from..from + len]));
            owner.push(index);
        }
    }
    // Write the pieces in order, stopping before the first part of each
    // crossing store to save what the guest has there. The stores before it
    // can have changed those bytes through another mapping of the same memory,
    // so they are read only once those stores are made.
    let mut next = 0;
    let mut saved: Option<(usize, Vec<u8>)> = None;
    let mut failed = None;
    for &(stop, readable) in crossing.iter().chain([(pieces.len(), false)].iter()) {
        let segment = &pieces[next..stop];
        let written = match write_guest_pieces(memory, buf, segment) {
            Ok(written) => written,
            Err(errno) => return (0, Err(errno)),
        };
        let mut end = 0;
        if let Some(at) = segment.iter().position(|(_, bytes)| {
            end += bytes.len();
            end > written
        }) {
            failed = Some(next + at);
            break;
        }
        if stop == pieces.len() {
            break;
        }
        let (at, bytes) = pieces[stop];
        let addr = unsafe { buf.add(at) };
        let mut before = vec![0; bytes.len()];
        let read = if readable {
            match read_guest_prefix(memory, addr, &mut before) {
                Ok(read) => read == before.len(),
                Err(errno) => return (0, Err(errno)),
            }
        } else {
            // The part in the second page, at its start, which no vectored copy
            // can read. Reverie's `read` of at most 8 bytes can on the ptrace
            // backend, through `PTRACE_PEEKDATA`, and a read changes nothing
            // the guest sees.
            memory.read_exact(addr, &mut before).is_ok()
        };
        saved = read.then_some((stop, before));
        next = stop;
    }
    let Some(failed) = failed else {
        return (names.len(), Ok(records.len()));
    };
    let store = owner[failed];
    if failed > 0
        && owner[failed - 1] == store
        && let Some((first, before)) = &saved
        && *first == failed - 1
        && let Err(errno) =
            write_guest_pieces(memory, buf, &[(pieces[*first].0, before.as_slice())])
    {
        return (0, Err(errno));
    }
    // The entry whose `filldir` made the store, or all of them for the call's
    // last store, and the first store of that `filldir`.
    let entry = filled.partition_point(|&end| end <= store);
    let first = entry.checked_sub(1).map_or(0, |before| filled[before]);
    let result = if entry == 0 || store == first {
        Err(Errno::EFAULT)
    } else {
        Ok(names[..entry]
            .iter()
            .map(|&name_len| format.record_len(name_len))
            .sum())
    };
    (entry, result)
}

/// A guest `getdents` or `getdents64` call.
#[derive(Clone, Copy)]
struct GetdentsCall<'a> {
    /// The call as the guest made it.
    call: Syscall,
    /// The same call asking for no bytes.
    empty: Syscall,
    fd: RawFd,
    buf: AddrMut<'a, u8>,
    /// The guest's buffer size.
    capacity: usize,
    format: DirentFormat,
}

impl<'a> GetdentsCall<'a> {
    /// The same call reading at most `count` bytes into `buf` instead.
    fn into_scratch<'b>(self, buf: AddrMut<'b, u8>, count: u32) -> GetdentsCall<'b> {
        let call = match self.call {
            Syscall::Getdents(call) => {
                Syscall::from(call.with_dirent(Some(buf.cast())).with_count(count))
            }
            Syscall::Getdents64(call) => {
                Syscall::from(call.with_dirent(Some(buf.cast())).with_count(count))
            }
            other => other,
        };
        GetdentsCall {
            call,
            empty: self.empty,
            fd: self.fd,
            buf,
            capacity: count as usize,
            format: self.format,
        }
    }
}

/// Whether every byte of `[address, address + len)` lies in some mapping of a
/// `/proc/<pid>/maps` listing (`Detcore::vectored_signalfd_read`).
fn signalfd_range_is_mapped(maps: &str, address: usize, len: usize) -> bool {
    signalfd_range_is_covered(maps, address, len, false)
}

/// Whether every byte of `[address, address + len)` lies in some writable
/// mapping of a `/proc/<pid>/maps` listing
/// (`Detcore::settle_faulted_signalfd_record`).
fn signalfd_range_is_writable(maps: &str, address: usize, len: usize) -> bool {
    signalfd_range_is_covered(maps, address, len, true)
}

/// The pieces of the guest buffers `extents` (address, length) that the
/// signalfd record after the first `read` bytes would fill, or `None` if
/// they have no room for a whole record there (Linux reads only whole ones).
fn signalfd_next_record(extents: &[(usize, usize)], read: usize) -> Option<Vec<(usize, usize)>> {
    const RECORD: usize = std::mem::size_of::<libc::signalfd_siginfo>();
    let capacity = extents
        .iter()
        .try_fold(0usize, |total, (_, len)| total.checked_add(*len))?;
    if capacity / RECORD <= read / RECORD {
        return None;
    }
    let mut skip = read - read % RECORD;
    let mut left = RECORD;
    let mut pieces = Vec::new();
    for (address, len) in extents {
        if left == 0 {
            break;
        }
        if skip >= *len {
            skip -= len;
            continue;
        }
        let take = (len - skip).min(left);
        pieces.push((address + skip, take));
        left -= take;
        skip = 0;
    }
    Some(pieces)
}

/// The end of the user address space, x86-64's `TASK_SIZE_MAX`, above which
/// `access_ok` fails a buffer before a call does anything: 2^56 - 4096 where
/// the kernel runs five-level page tables, 2^47 - 4096 otherwise. Measured
/// once, by asking for a mapping above 2^47, which the kernel grants there
/// only with five-level page tables.
fn user_address_end() -> usize {
    static END: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *END.get_or_init(|| {
        const HINT: usize = 1 << 48;
        // SAFETY: a fresh anonymous mapping, unmapped at once; the hint is
        // only a hint (no MAP_FIXED).
        let at = unsafe {
            libc::mmap(
                HINT as *mut libc::c_void,
                4096,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if at == libc::MAP_FAILED {
            return (1 << 47) - 4096;
        }
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(at, 4096) };
        if at as usize >= 1 << 47 {
            (1 << 56) - 4096
        } else {
            (1 << 47) - 4096
        }
    })
}

/// Authenticate the current task's FD/fdinfo directory by kernel object
/// identity, including symlink/bind aliases, without interpreting guest PIDs.
fn is_caller_descriptor_directory(
    observed: &std::fs::Metadata,
    caller: &str,
    process: Option<&str>,
) -> bool {
    [Some(caller), process].into_iter().flatten().any(|owner| {
        ["fd", "fdinfo"].into_iter().any(|kind| {
            std::fs::metadata(format!("{owner}/{kind}")).is_ok_and(|expected| {
                observed.dev() == expected.dev() && observed.ino() == expected.ino()
            })
        })
    })
}

fn runtime_owned_descriptor_entry(name: &[u8], owns: impl FnOnce(RawFd) -> bool) -> bool {
    !name.is_empty()
        && name.iter().all(u8::is_ascii_digit)
        && std::str::from_utf8(name)
            .ok()
            .and_then(|name| name.parse::<RawFd>().ok())
            .is_some_and(owns)
}

/// The signals a signalfd reads, from its `/proc/<pid>/fdinfo` entry.
fn signalfd_sigmask(pid: i32, fd: i32) -> Option<u64> {
    let info = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")).ok()?;
    let mask = info
        .lines()
        .find_map(|line| line.strip_prefix("sigmask:"))?;
    u64::from_str_radix(mask.trim(), 16).ok()
}

fn signalfd_range_is_covered(maps: &str, address: usize, len: usize, writable: bool) -> bool {
    let mut ranges: Vec<(usize, usize)> = maps
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (start, end) = fields.next()?.split_once('-')?;
            if writable && !fields.next()?.contains('w') {
                return None;
            }
            Some((
                usize::from_str_radix(start, 16).ok()?,
                usize::from_str_radix(end, 16).ok()?,
            ))
        })
        .collect();
    ranges.sort();
    let Some(end) = address.checked_add(len) else {
        return false;
    };
    let mut next = address;
    for (start, stop) in ranges {
        if start <= next && next < stop {
            next = stop;
            if next >= end {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod guest_proc_visibility_tests {
    use std::os::fd::AsRawFd;

    use reverie::BackendCapabilities;

    use super::reopens_scheduler_managed_pipe;
    use super::resolved_open_path;

    /// The open-path helpers read `/proc/<pid>/fd/<fd>` wherever guest
    /// threads are host tasks: ptrace (whose facts a DBT run without its
    /// configuration keeps), in-guest LiteInst, DBT and SaBRe.
    /// With this process's own pipe and file, those read the links. KVM, whose
    /// guest is not a host task, never consults them and gives the helpers'
    /// "unknown" answers.
    #[tokio::test]
    async fn host_proc_links_are_read_except_under_kvm() {
        let pid = std::process::id() as i32;
        let (reader, _writer) = nix::unistd::pipe().unwrap();
        let alias = nix::unistd::dup(&reader).unwrap();
        let file = std::fs::File::open("/proc/self/exe").unwrap();
        let (pipe_fd, alias_fd) = (reader.as_raw_fd(), alias.as_raw_fd());
        let file_fd = file.as_raw_fd();

        for backend in [
            BackendCapabilities::PTRACE,
            BackendCapabilities::LITEINST_IN_GUEST,
            BackendCapabilities::DBT,
            BackendCapabilities::SABRE,
        ] {
            assert!(
                reopens_scheduler_managed_pipe(&backend, pid, alias_fd, &[pipe_fd]),
                "{backend:?}"
            );
            assert!(
                resolved_open_path(&backend, pid, file_fd).is_some(),
                "{backend:?}"
            );
        }
        let kvm = BackendCapabilities::KVM;
        assert!(!reopens_scheduler_managed_pipe(
            &kvm,
            pid,
            alias_fd,
            &[pipe_fd]
        ));
        assert_eq!(resolved_open_path(&kvm, pid, file_fd), None);

        // Runtime ownership is exact: ordinary high application descriptors
        // remain visible, and a directory merely named "fd" is not procfs.
        for name in [
            b"1023".as_slice(),
            b"1025",
            b"4096",
            b".",
            b"..",
            b"+1024",
            b"2147483648",
        ] {
            assert!(!super::runtime_owned_descriptor_entry(name, |fd| fd == 1024));
        }
        assert!(super::runtime_owned_descriptor_entry(b"1024", |fd| fd == 1024));
        assert!(!super::runtime_owned_descriptor_entry(b"1024", |_| false));
        for kind in ["fd", "fdinfo"] {
            let thread = std::fs::metadata(format!("/proc/thread-self/{kind}")).unwrap();
            let process = std::fs::metadata(format!("/proc/self/{kind}")).unwrap();
            assert!(super::is_caller_descriptor_directory(
                &thread,
                "/proc/thread-self",
                None
            ));
            assert!(!super::is_caller_descriptor_directory(
                &process,
                "/proc/thread-self",
                None
            ));
            assert!(super::is_caller_descriptor_directory(
                &process,
                "/proc/thread-self",
                Some("/proc/self")
            ));
        }
        let ordinary = tempfile::tempdir().unwrap();
        let metadata = std::fs::metadata(ordinary.path()).unwrap();
        assert!(!super::is_caller_descriptor_directory(
            &metadata,
            "/proc/thread-self",
            Some("/proc/self")
        ));
        super::inject_fstat_scratch::raw_directory_projection().await;
    }
}

#[cfg(test)]
mod signalfd_geometry_tests {
    use super::signalfd_next_record;
    use super::signalfd_range_is_mapped;
    use super::signalfd_range_is_writable;

    /// A range is mapped only when contiguous mappings cover every byte.
    #[test]
    fn an_iovec_array_is_mapped_only_when_mappings_cover_every_byte() {
        let maps =
            "1000-2000 r--p 0 0:0 0\n2000-3000 ---p 0 0:0 0 [vvar]\n5000-6000 rw-p 0 0:0 0\n";
        assert!(signalfd_range_is_mapped(maps, 0x1ff0, 32));
        assert!(
            !signalfd_range_is_mapped(maps, 0x2ff0, 32),
            "runs into a hole"
        );
        assert!(!signalfd_range_is_mapped(maps, 0x4000, 16));
        assert!(!signalfd_range_is_mapped(maps, usize::MAX - 4, 16));
        assert!(!signalfd_range_is_mapped("", 0x1000, 16));
    }

    /// The record a signalfd read could have lost after the bytes it
    /// returned: only where the buffers hold a whole further record, which
    /// may span iovecs, and lost only if some byte of it is not writable.
    #[test]
    fn the_record_after_a_read_is_where_the_buffers_have_room_for_it() {
        assert_eq!(
            signalfd_next_record(&[(0x1000, 256)], 128),
            Some(vec![(0x1080, 128)])
        );
        assert_eq!(signalfd_next_record(&[(0x1000, 255)], 128), None);
        assert_eq!(signalfd_next_record(&[(0x1000, 128)], 128), None);
        assert_eq!(
            signalfd_next_record(&[(0x1000, 100), (0x3000, 60), (0x5000, 300)], 128),
            Some(vec![(0x3000 + 28, 32), (0x5000, 96)])
        );
        assert_eq!(
            signalfd_next_record(&[(0x1000, usize::MAX), (0x2000, 2)], 0),
            None
        );
        let maps = "1000-2000 rw-p 0 0:0 0\n2000-3000 ---p 0 0:0 0\n";
        assert!(signalfd_range_is_writable(maps, 0x1f80, 128));
        assert!(!signalfd_range_is_writable(maps, 0x1fc0, 128));
        assert!(signalfd_range_is_mapped(maps, 0x1fc0, 128));
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

    /// Both mountinfo captures in the snapshot initializer must drop ephemeral
    /// host seed rows (https://github.com/rrnewton/hermit/pull/3219): the guest
    /// read's capture, for the files that carry mountinfo identities, and the
    /// tracer-side capture that validates fdinfo `mnt_id`. Without either, host
    /// seed churn reaches guest-visible mountinfo or shifts mount-ID assignment,
    /// and no unit test of the filter itself notices.
    #[test]
    fn snapshot_initializer_excludes_host_seed_mounts_at_both_captures() {
        let body = handler_body("initialize_procfs_snapshot");
        let calls = body.matches("exclude_ephemeral_host_seed_mounts(").count();
        assert_eq!(
            calls, 2,
            "MISSING MECHANISM: initialize_procfs_snapshot must call \
             exclude_ephemeral_host_seed_mounts exactly twice (the guest mountinfo \
             capture and the fdinfo mnt_id capture); found {calls}. Without it, \
             unrelated host squashfuse seed mounts make /proc/<pid>/mountinfo and \
             fdinfo mount IDs depend on host timing."
        );
        let guest_capture = body
            .find("exclude_ephemeral_host_seed_mounts(&raw_contents)")
            .expect(
                "MISSING MECHANISM: the guest read's procfs capture (`raw_contents`) is \
                 no longer filtered for host seed mounts",
            );
        let gate = body[..guest_capture]
            .rfind("let contents = if")
            .map(|start| &body[start..guest_capture])
            .unwrap_or_default();
        assert!(
            gate.contains("procfs_needs_mountinfo_identities()"),
            "MISSING MECHANISM: the guest capture's seed filter must apply exactly to \
             the files that carry mountinfo identities (`let contents = if ... \
             procfs_needs_mountinfo_identities()`); found {gate:?}"
        );
        let fdinfo_capture = body
            .find("exclude_ephemeral_host_seed_mounts(&mountinfo_contents)")
            .expect(
                "MISSING MECHANISM: the fdinfo mnt_id capture (`mountinfo_contents`) is \
                 no longer filtered for host seed mounts",
            );
        let parse = body
            .find("parse_mountinfo(&mountinfo_contents)")
            .expect("the fdinfo mnt_id capture no longer parses mountinfo_contents");
        assert!(
            fdinfo_capture < parse,
            "MISSING MECHANISM: the fdinfo mnt_id capture parses mountinfo before \
             dropping host seed rows"
        );
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
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::Whence;

    use super::DETERMINISTIC_PIPE_CAPACITY_BYTES;
    use super::fcntl_host_timed_signals;
    use super::ioctl_host_timed_signals;
    use super::may_have_acquired_controlling_terminal;
    use super::open_can_acquire_controlling_terminal;
    use super::opened_controlling_terminal;
    use super::pipe_capacity_request_exceeds_ceiling;
    use super::terminal_can_become_controlling;
    use crate::test_pages::PAGE;
    use crate::test_pages::Pages;

    /// A `/proc/self` or `/proc/thread-self` spelling leads to the opener's
    /// task, which a name relative to the descriptor is then looked up below,
    /// whoever looks it up.
    #[test]
    fn a_proc_selector_leads_to_the_openers_task() {
        use std::path::Path;

        use super::procfs_selector_reached;

        let own = Some((3, 5));
        for (spelling, reached) in [
            ("/proc/self", Some("/proc/3")),
            ("/proc/self/", Some("/proc/3")),
            ("/proc/self/task", Some("/proc/3/task")),
            ("/proc/self/task/5", Some("/proc/3/task/5")),
            ("/proc/thread-self", Some("/proc/3/task/5")),
            ("/proc/self/fd/7", None),
            ("/proc/self/task/05", None),
            ("/proc/thread-self/fd/7", None),
            ("/proc/self/cwd", None),
            ("/proc/3", Some("/proc/3")),
            ("/proc/7/", Some("/proc/7")),
            ("/proc/7/task", Some("/proc/7/task")),
            ("/proc/7/task/9", Some("/proc/7/task/9")),
            ("/proc/07", None),
            ("/proc/7/task/09", None),
            ("/proc/7/fd", None),
            ("/proc/7/task/9/fd/1", None),
            ("/proc/sys", None),
            ("/proc", None),
            ("/proc/selfish", None),
            ("/dev/fd", None),
            ("self", None),
        ] {
            assert_eq!(
                procfs_selector_reached(Path::new(spelling), own).as_deref(),
                reached.map(Path::new),
                "{spelling}"
            );
        }
        // In a nested PID namespace Detcore does not know the caller's numbers.
        assert_eq!(procfs_selector_reached(Path::new("/proc/self"), None), None);
        assert_eq!(procfs_selector_reached(Path::new("/proc/3"), None), None);
        // The selector links themselves, which an `O_PATH | O_NOFOLLOW` open
        // opens rather than the directory they lead to.
        for (spelling, link) in [
            ("/proc/self", true),
            ("/proc/thread-self", true),
            // A trailing slash follows the link (`opens_the_link_itself`).
            ("/proc/self/", true),
            ("/proc/self/task", false),
            ("/proc/3", false),
            ("/proc/selfish", false),
        ] {
            assert_eq!(
                super::names_a_selector_link(Path::new(spelling)),
                link,
                "{spelling}"
            );
        }
        // The task each reached directory is, or is below.
        let task = |process, thread| crate::procfs::BoundTask { process, thread };
        for (reached, expected) in [
            ("/proc/3", Some(task(3, None))),
            ("/proc/3/task", Some(task(3, None))),
            ("/proc/3/task/5", Some(task(3, Some(5)))),
            ("/proc/3/task/5/ns", Some(task(3, Some(5)))),
            ("/proc/3/ns", Some(task(3, None))),
            ("/proc", None),
            ("/proc/sys", None),
            ("/proc/0", None),
        ] {
            assert_eq!(
                super::procfs_reached_task(Path::new(reached)),
                expected,
                "{reached}"
            );
        }
    }

    /// A descriptor opened through one of the caller's descriptor links
    /// records the path of the descriptor the link names, so a name relative
    /// to it is the file Linux finds below that file: `/proc/stat` through a
    /// reopened `/proc`, and through a reopened reopening. It keeps its
    /// spelling wherever the recorded path could name another file: a link
    /// opened itself (`O_NOFOLLOW` without a trailing slash), the process's
    /// table where the caller holds a table of its own, a target without an
    /// absolute recorded path, and a target that names a per-caller
    /// selector. Every other open records its spelling.
    #[test]
    fn an_open_through_a_descriptor_link_records_the_targets_path() {
        use std::path::Path;
        use std::path::PathBuf;

        use super::DescriptorLinkOpener;
        use super::path_to_record_for_open;

        let leader = DescriptorLinkOpener {
            own: Some((3, 3)),
            holds_process_table: true,
        };
        let read = OFlag::O_RDONLY | OFlag::O_DIRECTORY;
        let table = |fd: i32| match fd {
            4 => Some(PathBuf::from("/proc")),
            6 => Some(PathBuf::from("relative/dir")),
            8 => Some(PathBuf::from("/proc/self")),
            9 => Some(PathBuf::from("/proc/thread-self")),
            10 => Some(PathBuf::from("/dev/fd")),
            11 => Some(PathBuf::from("/proc/12")),
            _ => None,
        };
        for spelling in [
            "/proc/self/fd/4",
            "/dev/fd/4",
            "/proc/thread-self/fd/4",
            "/proc/3/fd/4",
            "/proc/3/task/3/fd/4",
        ] {
            let recorded = path_to_record_for_open(Path::new(spelling), read, leader, table, None);
            assert_eq!(recorded, Path::new("/proc"), "{spelling}");
            // A dirfd-relative name below the new descriptor is then the
            // rendered file's own path (`Detcore::observed_path`).
            assert_eq!(recorded.join("stat"), Path::new("/proc/stat"));
            assert_eq!(
                recorded.join("sys/kernel/random/uuid"),
                Path::new("/proc/sys/kernel/random/uuid")
            );
        }
        // An alias of an alias: the second open finds the first's recorded
        // path, already the target's.
        let first =
            path_to_record_for_open(Path::new("/proc/self/fd/4"), read, leader, table, None);
        let second = path_to_record_for_open(
            Path::new("/dev/fd/9"),
            read,
            leader,
            |fd| (fd == 9).then(|| first.clone()),
            None,
        );
        assert_eq!(second, Path::new("/proc"));
        // O_NOFOLLOW opens the link itself (O_PATH) unless a trailing slash
        // or `/.` follows it.
        let nofollow = OFlag::O_PATH | OFlag::O_NOFOLLOW;
        assert_eq!(
            path_to_record_for_open(Path::new("/proc/self/fd/4"), nofollow, leader, table, None),
            Path::new("/proc/self/fd/4")
        );
        for spelling in ["/proc/self/fd/4/", "/proc/self/fd/4/.", "/dev/fd/4/"] {
            assert_eq!(
                path_to_record_for_open(Path::new(spelling), nofollow, leader, table, None),
                Path::new("/proc"),
                "{spelling}"
            );
        }
        // A thread with a table of its own: the process's links name the
        // leader's table, not the caller's, so they keep the spelling; its
        // own thread links still name its table.
        let split = DescriptorLinkOpener {
            own: Some((3, 5)),
            holds_process_table: false,
        };
        for spelling in ["/proc/self/fd/4", "/dev/fd/4", "/proc/3/fd/4"] {
            assert_eq!(
                path_to_record_for_open(
                    Path::new(spelling),
                    read,
                    split,
                    |_| { panic!("{spelling} names the leader's table, not the caller's") },
                    None
                ),
                Path::new(spelling)
            );
        }
        for spelling in ["/proc/thread-self/fd/4", "/proc/3/task/5/fd/4"] {
            assert_eq!(
                path_to_record_for_open(Path::new(spelling), read, split, table, None),
                Path::new("/proc"),
                "{spelling}"
            );
        }
        // A target without a recorded path, with a relative one, or with one
        // procfs resolves per caller, and another process's link, keep the
        // spelling. A numeric /proc/<pid> target is not per caller.
        for spelling in [
            "/proc/self/fd/5",
            "/proc/self/fd/6",
            "/proc/self/fd/8",
            "/proc/self/fd/9",
            "/dev/fd/10",
            "/proc/7/fd/4",
        ] {
            assert_eq!(
                path_to_record_for_open(Path::new(spelling), read, leader, table, None),
                Path::new(spelling)
            );
        }
        assert_eq!(
            path_to_record_for_open(Path::new("/dev/fd/11"), read, leader, table, None),
            Path::new("/proc/12")
        );
        // So does every open that names no descriptor link.
        for spelling in ["/proc/stat", "/proc/self/fd/4/stat", "/tmp/x"] {
            assert_eq!(
                path_to_record_for_open(
                    Path::new(spelling),
                    read,
                    leader,
                    |_| { panic!("{spelling} names no descriptor") },
                    None
                ),
                Path::new(spelling)
            );
        }
        // The path is N's as the open's lookup found it, and only while N
        // holds the same open file description: an open of N's link that
        // waited (a FIFO) while a thread sharing the table replaced N with a
        // /proc descriptor opened the FIFO, so it is not recorded as /proc.
        use super::LinkSource;
        let creator = crate::types::DetTid::from_raw(3);
        let fifo = LinkSource {
            fd: 5,
            path: Some(PathBuf::from("/tmp/dir/fifo")),
            open_file: crate::types::OpenFileId::new(creator, 7),
            reached: None,
        };
        let replaced = Some(crate::types::OpenFileId::new(creator, 8));
        let unchanged = Some(fifo.open_file);
        let spelling = Path::new("/proc/self/fd/5");
        assert_eq!(
            path_to_record_for_open(
                spelling,
                read,
                leader,
                |fd| fifo.path_if_unchanged(fd, unchanged),
                None
            ),
            Path::new("/tmp/dir/fifo")
        );
        assert_eq!(
            path_to_record_for_open(
                spelling,
                read,
                leader,
                |fd| fifo.path_if_unchanged(fd, replaced),
                None
            ),
            spelling
        );
        assert_eq!(
            path_to_record_for_open(
                spelling,
                read,
                leader,
                |fd| fifo.path_if_unchanged(fd, None),
                None
            ),
            spelling
        );
        assert_eq!(fifo.path_if_unchanged(4, unchanged), None);
    }

    /// Where the descriptor a link names has no recorded path (one received
    /// through `SCM_RIGHTS`, which Detcore does not track), an open through
    /// the link records the procfs directory the kernel names for the opened
    /// descriptor (`Detcore::procfs_directory_named_for`): `/proc`, below
    /// which `stat` is `/proc/stat`. Without that name it keeps the
    /// spelling, as before. A recorded path of `N` still comes first, and the
    /// name is never taken for a link opened itself, a spelling that names no
    /// descriptor link, or a link to a table the caller does not hold. Only
    /// `/proc` and the pid-free paths below it are names the kernel's link
    /// may give (`procfs_directory_link`).
    #[test]
    fn an_open_through_an_unrecorded_descriptors_link_records_its_identity() {
        use std::path::Path;
        use std::path::PathBuf;

        use super::DescriptorLinkOpener;
        use super::path_to_record_for_open;

        let leader = DescriptorLinkOpener {
            own: Some((3, 3)),
            holds_process_table: true,
        };
        let read = OFlag::O_RDONLY | OFlag::O_DIRECTORY;
        let proc = || Some(PathBuf::from("/proc"));
        for spelling in [
            "/proc/self/fd/10",
            "/dev/fd/10",
            "/proc/thread-self/fd/10",
            "/proc/3/fd/10",
            "/proc/3/task/3/fd/10",
        ] {
            let recorded =
                path_to_record_for_open(Path::new(spelling), read, leader, |_| None, proc());
            assert_eq!(recorded, Path::new("/proc"), "{spelling}");
            assert_eq!(recorded.join("stat"), Path::new("/proc/stat"));
            assert_eq!(
                path_to_record_for_open(Path::new(spelling), read, leader, |_| None, None),
                Path::new(spelling)
            );
        }
        assert_eq!(
            path_to_record_for_open(
                Path::new("/proc/self/fd/4"),
                read,
                leader,
                |fd| (fd == 4).then(|| PathBuf::from("/proc/12")),
                proc(),
            ),
            Path::new("/proc/12")
        );
        let nofollow = OFlag::O_PATH | OFlag::O_NOFOLLOW;
        assert_eq!(
            path_to_record_for_open(
                Path::new("/proc/self/fd/10"),
                nofollow,
                leader,
                |_| None,
                proc()
            ),
            Path::new("/proc/self/fd/10")
        );
        for spelling in ["/proc/self/fd/10/stat", "/tmp/x"] {
            assert_eq!(
                path_to_record_for_open(Path::new(spelling), read, leader, |_| None, proc()),
                Path::new(spelling)
            );
        }
        // A recorded path refused as per caller is not replaced by the name.
        assert_eq!(
            path_to_record_for_open(
                Path::new("/proc/self/fd/8"),
                read,
                leader,
                |_| Some(PathBuf::from("/proc/self")),
                proc(),
            ),
            Path::new("/proc/self/fd/8")
        );
        // A thread with a table of its own: the process's links name the
        // leader's table, whose N the caller's table does not describe, so
        // they keep the spelling; its own thread links take the name.
        let split = DescriptorLinkOpener {
            own: Some((3, 5)),
            holds_process_table: false,
        };
        for spelling in ["/proc/self/fd/10", "/dev/fd/10", "/proc/3/fd/10"] {
            assert_eq!(
                path_to_record_for_open(Path::new(spelling), read, split, |_| None, proc()),
                Path::new(spelling)
            );
        }
        for spelling in ["/proc/thread-self/fd/10", "/proc/3/task/5/fd/10"] {
            assert_eq!(
                path_to_record_for_open(Path::new(spelling), read, split, |_| None, proc()),
                Path::new("/proc"),
                "{spelling}"
            );
        }
        use super::procfs_directory_link;
        for accepted in ["/proc", "/proc/sys", "/proc/sys/kernel", "/proc/sys/net"] {
            assert!(procfs_directory_link(Path::new(accepted)), "{accepted}");
        }
        for refused in [
            "/tmp/dir",
            "/procfs",
            "/proc (deleted)",
            "/proc/sys (deleted)",
            "/proc/12",
            "/proc/12/task/12",
            "/proc/self",
            "/proc/thread-self",
            // A symbolic link to the caller's own /proc/self/net.
            "/proc/net",
            "/proc/sys/../12",
            "proc",
            "anon_inode:[eventfd]",
            "/memfd:x (deleted)",
        ] {
            assert!(!procfs_directory_link(Path::new(refused)), "{refused}");
        }
    }

    /// The kernel's name for a received procfs directory gives its components
    /// below `/proc` and the positions of the task numbers among them, which
    /// DBT translates to the guest's: a process's directory and a thread's
    /// below its `task`, nothing for the system directories, and no name
    /// procfs would not give.
    #[test]
    fn a_received_procfs_directory_names_its_task_positions() {
        use super::procfs_directory_task_positions;
        let parsed = |link: &str| procfs_directory_task_positions(link);
        let owned = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
        assert_eq!(parsed("/proc"), Some((Vec::new(), Vec::new())));
        assert_eq!(
            parsed("/proc/sys/kernel"),
            Some((owned(&["sys", "kernel"]), Vec::new()))
        );
        assert_eq!(parsed("/proc/12"), Some((owned(&["12"]), vec![0])));
        assert_eq!(parsed("/proc/12/ns"), Some((owned(&["12", "ns"]), vec![0])));
        assert_eq!(parsed("/proc/12/fd"), Some((owned(&["12", "fd"]), vec![0])));
        assert_eq!(
            parsed("/proc/12/task/14"),
            Some((owned(&["12", "task", "14"]), vec![0, 2]))
        );
        assert_eq!(
            parsed("/proc/12/task/14/ns"),
            Some((owned(&["12", "task", "14", "ns"]), vec![0, 2]))
        );
        assert_eq!(
            parsed("/proc/irq/5"),
            Some((owned(&["irq", "5"]), Vec::new()))
        );
        for refused in [
            "/proc/012",
            "/proc/12/task/014",
            "/procfs",
            "/tmp/12",
            "proc/12",
            "/proc/12 (deleted)",
            "socket:[1]",
            "/proc/12/../13",
        ] {
            assert_eq!(parsed(refused), None, "{refused}");
        }
    }

    #[test]
    fn inherited_input_status_flags_remain_kernel_owned() {
        for recordreplay_modes in [false, true] {
            let tool = crate::Detcore {
                detpid: crate::types::DetPid::from_raw(3),
                cfg: crate::Config {
                    sequentialize_threads: true,
                    recordreplay_modes,
                    ..Default::default()
                },
                record_or_replay: crate::record_or_replay::NoopTool,
            };
            for physically_nonblocking in [false, true] {
                assert!(!tool.keeps_physically_nonblocking(
                    crate::fd::FdType::Pipe,
                    false,
                    physically_nonblocking,
                    true
                ));
                assert_eq!(
                    tool.keeps_physically_nonblocking(
                        crate::fd::FdType::Pipe,
                        false,
                        physically_nonblocking,
                        false
                    ),
                    !recordreplay_modes || physically_nonblocking
                );
                assert_eq!(
                    tool.keeps_physically_nonblocking(
                        crate::fd::FdType::Socket,
                        true,
                        physically_nonblocking,
                        false
                    ),
                    !recordreplay_modes || physically_nonblocking
                );
            }
        }
    }

    /// Every `fcntl` and `ioctl` that can make Linux signal a descriptor's
    /// owner at a moment set by host timing names SIGIO and SIGURG, plus the
    /// signal `F_SETSIG` chooses; every other command names nothing
    /// (https://github.com/rrnewton/hermit/issues/3146).
    #[test]
    fn async_io_arming_calls_name_their_host_timed_signals() {
        use reverie::syscalls::FcntlCmd;
        use reverie::syscalls::ioctl::Request;
        let bit = |signal: i32| 1_u64 << (signal - 1);
        let async_io = bit(libc::SIGIO) | bit(libc::SIGURG);
        for (cmd, expected) in [
            (
                FcntlCmd::F_SETFL(libc::O_ASYNC | libc::O_NONBLOCK),
                async_io,
            ),
            (FcntlCmd::F_SETFL(libc::O_NONBLOCK), 0),
            (FcntlCmd::F_SETOWN, async_io),
            (FcntlCmd::F_SETOWN_EX(None), async_io),
            (FcntlCmd::F_SETLEASE(libc::F_RDLCK), async_io),
            // DN_MODIFY, which the libc crate does not export.
            (FcntlCmd::F_NOTIFY(0x2), async_io),
            (
                FcntlCmd::F_SETSIG(libc::SIGUSR1),
                async_io | bit(libc::SIGUSR1),
            ),
            (FcntlCmd::F_SETSIG(0), async_io),
            (FcntlCmd::F_GETFL, 0),
            (FcntlCmd::F_SETFD(libc::FD_CLOEXEC), 0),
        ] {
            assert_eq!(fcntl_host_timed_signals(cmd), expected, "{cmd:?}");
        }
        for (request, expected) in [
            (Request::FIOASYNC(None), async_io),
            (Request::FIOSETOWN(None), async_io),
            (Request::SIOCSPGRP(None), async_io),
            (Request::FIOCLEX, 0),
        ] {
            assert_eq!(ioctl_host_timed_signals(request), expected, "{request:?}");
        }
    }

    /// The calls that give a session its controlling terminal (TIOCSCTTY, and
    /// TIOCGPTPEER without O_NOCTTY, which opens a pseudoterminal's other end
    /// as `open` does) or choose the terminal's foreground process group
    /// (TIOCSPGRP) name every signal Linux sends because of that terminal, at
    /// a moment set by host timing: SIGHUP and SIGCONT when it hangs up or the
    /// leader exits, SIGINT, SIGQUIT and SIGTSTP for its interrupt, quit and
    /// suspend characters, SIGWINCH when its size changes, and SIGTTIN and
    /// SIGTTOU when a background process group reads or writes it. Reading the
    /// foreground group (TIOCGPGRP), giving up the controlling terminal
    /// (TIOCNOTTY), opening the other end with O_NOCTTY, and setting the size
    /// (TIOCSWINSZ, whose SIGWINCH is sent inside the caller's call) name
    /// nothing (round-9 High 2 and the round-11 finding "Terminal resize
    /// remains an admitted host-timed interruption" of
    /// https://github.com/rrnewton/hermit/pull/3361).
    #[test]
    fn terminal_control_calls_name_the_terminal_signals() {
        use reverie::syscalls::ioctl::Request;
        let bit = |signal: i32| 1_u64 << (signal - 1);
        let terminal = [
            libc::SIGHUP,
            libc::SIGCONT,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTSTP,
            libc::SIGWINCH,
            libc::SIGTTIN,
            libc::SIGTTOU,
        ]
        .into_iter()
        .fold(0, |set, signal| set | bit(signal));
        for (request, expected) in [
            (Request::TIOCSCTTY(0), terminal),
            (Request::TIOCSCTTY(1), terminal),
            (Request::TIOCSPGRP(None), terminal),
            (Request::TIOCGPTPEER(libc::O_RDWR), terminal),
            (
                Request::TIOCGPTPEER(libc::O_RDWR | libc::O_CLOEXEC),
                terminal,
            ),
            (Request::TIOCGPTPEER(libc::O_RDWR | libc::O_NOCTTY), 0),
            (Request::TIOCGPGRP(None), 0),
            (Request::TIOCNOTTY, 0),
            (Request::TIOCSWINSZ(None), 0),
        ] {
            assert_eq!(ioctl_host_timed_signals(request), expected, "{request:?}");
        }
    }

    /// An `open` can give its caller a controlling terminal only without
    /// O_NOCTTY and O_PATH, only in a session leader, and only of a terminal
    /// Linux can make controlling. None of the inputs is the controlling
    /// terminal procfs lists, which a hangup clears
    /// (https://github.com/rrnewton/hermit/issues/3146,
    /// https://github.com/rrnewton/hermit/pull/3361).
    #[test]
    fn an_open_acquires_a_controlling_terminal_only_as_a_session_leader() {
        assert!(open_can_acquire_controlling_terminal(OFlag::O_RDWR));
        assert!(!open_can_acquire_controlling_terminal(
            OFlag::O_RDWR | OFlag::O_NOCTTY
        ));
        assert!(!open_can_acquire_controlling_terminal(OFlag::O_PATH));
        // A host's /proc/tty/drivers, plus a driver whose name has a space.
        let table = Some(
            "/dev/tty             /dev/tty        5       0 system:/dev/tty\n\
             /dev/console         /dev/console    5       1 system:console\n\
             /dev/ptmx            /dev/ptmx       5       2 system\n\
             /dev/vc/0            /dev/vc/0       4       0 system:vtmaster\n\
             serial_8250          /dev/ttyS       4 64-95 serial\n\
             acm serial           /dev/ttyACM   166 0-255 serial\n\
             pty_slave            /dev/pts      136 0-1048575 pty:slave\n\
             pty_master           /dev/ptm      128 0-1048575 pty:master\n\
             unknown              /dev/tty        4 1-63 console\n",
        );
        // /dev/pts/3 is character device 136:3; minor 300 sets bits above 0xff.
        let pts3 = libc::makedev(136, 3);
        let pts300 = libc::makedev(136, 300);
        assert!(may_have_acquired_controlling_terminal(42, 42, pts3, table));
        assert!(may_have_acquired_controlling_terminal(
            42, 42, pts300, table
        ));
        assert!(!may_have_acquired_controlling_terminal(43, 42, pts3, table));
        // Serial lines, virtual consoles and /dev/tty can become controlling;
        // /dev/console, /dev/ptmx, /dev/vc/0, pseudoterminal masters, minors
        // outside every range and devices that are not terminals cannot.
        for (major, minor) in [(4, 64), (4, 95), (166, 7), (4, 1), (4, 63), (5, 0)] {
            let rdev = libc::makedev(major, minor);
            assert!(
                terminal_can_become_controlling(rdev, table),
                "{major}:{minor}"
            );
        }
        for (major, minor) in [(5, 1), (5, 2), (4, 0), (128, 3), (4, 96), (188, 0), (1, 3)] {
            let rdev = libc::makedev(major, minor);
            assert!(
                !terminal_can_become_controlling(rdev, table),
                "{major}:{minor}"
            );
        }
        // A table that cannot be read answers true, except for the devices
        // Linux never makes controlling. A line that does not parse answers
        // true, unless another line names the device.
        let null = libc::makedev(1, 3);
        assert!(terminal_can_become_controlling(pts3, None));
        assert!(terminal_can_become_controlling(null, None));
        assert!(!terminal_can_become_controlling(libc::makedev(5, 1), None));
        let garbled = Some("pty_slave /dev/pts 136 0-1048575 pty:slave\nnot a driver line\n");
        assert!(terminal_can_become_controlling(null, garbled));
        assert!(terminal_can_become_controlling(pts3, garbled));
        let master = Some("pty_master /dev/ptm 128 0-1048575 pty:master\nnot a driver line\n");
        assert!(!terminal_can_become_controlling(
            libc::makedev(128, 3),
            master
        ));
    }

    /// A session leader that opens a pseudoterminal's slave without O_NOCTTY
    /// gains it as its controlling terminal. Closing the master then hangs the
    /// slave up, which clears the controlling terminal procfs lists but changes
    /// neither the session nor the descriptor's device. A hangup that lands
    /// after the `open`, whether before or after the descriptor's stat, must
    /// not hide that the `open` may have acquired the terminal
    /// (https://github.com/rrnewton/hermit/pull/3361).
    #[test]
    fn a_hangup_after_the_open_does_not_hide_the_acquired_terminal() {
        use std::time::Duration;
        use std::time::Instant;

        struct Child(libc::pid_t);
        impl Drop for Child {
            fn drop(&mut self) {
                // SAFETY: the pid is this test's own child, not yet reaped.
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    libc::waitpid(self.0, std::ptr::null_mut(), 0);
                }
            }
        }

        // SAFETY (every block below): libc calls on descriptors, buffers and
        // a child process this test owns.
        let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
        let master = unsafe { libc::posix_openpt(flags) };
        assert!(
            master >= 0,
            "posix_openpt: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(unsafe { libc::grantpt(master) }, 0);
        assert_eq!(unsafe { libc::unlockpt(master) }, 0);
        let mut slave_path = [0 as libc::c_char; 64];
        let path_len = slave_path.len();
        assert_eq!(
            unsafe { libc::ptsname_r(master, slave_path.as_mut_ptr(), path_len) },
            0
        );
        // `ready` carries the slave's descriptor number to this process;
        // nothing writes `release`, so this process's exit ends the child.
        let (mut ready, mut release) = ([0; 2], [0; 2]);
        assert_eq!(
            unsafe { libc::pipe2(ready.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        assert_eq!(
            unsafe { libc::pipe2(release.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            // The child makes only async-signal-safe calls and allocates
            // nothing, so forking from a test harness thread is safe.
            unsafe {
                libc::alarm(60);
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                libc::close(master);
                libc::close(ready[0]);
                libc::close(release[1]);
                if libc::setsid() < 0 {
                    libc::_exit(2);
                }
                let slave = libc::open(slave_path.as_ptr(), libc::O_RDWR);
                if slave < 0 {
                    libc::_exit(3);
                }
                let bytes = slave.to_ne_bytes();
                if libc::write(ready[1], bytes.as_ptr().cast(), bytes.len()) != 4 {
                    libc::_exit(4);
                }
                let mut byte = [0_u8; 1];
                libc::read(release[0], byte.as_mut_ptr().cast(), 1);
                libc::_exit(0);
            }
        }
        let child = Child(pid);
        unsafe {
            libc::close(ready[1]);
            libc::close(release[0]);
        }
        let mut bytes = [0_u8; 4];
        let got = unsafe { libc::read(ready[0], bytes.as_mut_ptr().cast(), bytes.len()) };
        assert_eq!(got, 4, "the child did not open the slave");
        let slave = i32::from_ne_bytes(bytes);
        // The descriptor's stat, read before the hangup, as `handle_openat`
        // reads it before it classifies the open.
        let link = std::ffi::CString::new(format!("/proc/{pid}/fd/{slave}")).unwrap();
        let mut host_stat: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::stat(link.as_ptr(), &mut host_stat) }, 0);
        let terminal = (
            libc::major(host_stat.st_rdev) as i32,
            libc::minor(host_stat.st_rdev) as i32,
        );
        let process = procfs::process::Process::new(pid).unwrap();
        let before = process.stat().unwrap();
        assert_eq!(
            (before.session, before.tty_nr()),
            (pid, terminal),
            "the child's open did not acquire the terminal"
        );
        assert!(opened_controlling_terminal(pid, slave, Some(&host_stat)));
        // Hang the slave up: Linux clears the session's controlling terminal.
        assert_eq!(unsafe { libc::close(master) }, 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        while process.stat().unwrap().tty_nr() != (0, 0) {
            assert!(
                Instant::now() < deadline,
                "the hangup did not clear the controlling terminal"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(process.stat().unwrap().session, pid);
        assert!(
            opened_controlling_terminal(pid, slave, Some(&host_stat)),
            "a hangup after the descriptor's stat hid the acquisition"
        );
        assert!(
            opened_controlling_terminal(pid, slave, None),
            "a hangup before the descriptor's stat hid the acquisition"
        );
        drop(child);
    }

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

    /// A host pipe's capacity above the ceiling reads as the ceiling, so a
    /// default 64-KiB pipe and one the host shrank to two pages under
    /// `pipe-user-pages-soft` read the same. A capacity at or below the ceiling
    /// (Hermit's pin, or a guest's own shrink) reads as it is.
    #[test]
    fn a_host_pipe_capacity_reads_no_larger_than_the_ceiling() {
        let ceiling = i64::from(DETERMINISTIC_PIPE_CAPACITY_BYTES);
        assert_eq!(super::reported_pipe_capacity(65536), ceiling);
        assert_eq!(super::reported_pipe_capacity(1 << 20), ceiling);
        assert_eq!(super::reported_pipe_capacity(ceiling), ceiling);
        assert_eq!(super::reported_pipe_capacity(4096), 4096);
    }

    /// Shrinking stays legal. `tests/c/pipe_capacity.c`
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
    use super::should_tag_host_timed_internal_pipe_io;
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
        assert!(should_tag_host_timed_internal_pipe_io(
            true,
            FdType::Pipe,
            true,
            false
        ));
        assert!(!should_tag_host_timed_internal_pipe_io(
            true,
            FdType::Pipe,
            true,
            true
        ));
        assert!(!should_tag_host_timed_internal_pipe_io(
            true,
            FdType::Pipe,
            false,
            false
        ));
        assert!(!should_tag_host_timed_internal_pipe_io(
            false,
            FdType::Pipe,
            true,
            false
        ));
        assert!(!should_tag_host_timed_internal_pipe_io(
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

    /// Records with the names `names`, as `serve_next_batch` encodes them
    /// from position 0, and the length of each name.
    fn directory_records(format: super::DirentFormat, names: &[&[u8]]) -> (Vec<u8>, Vec<usize>) {
        let mut records = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let entry = super::DirEntry {
                name: name.to_vec(),
                ino: 0x1111_1111_1111_1111,
                off: 0,
                ty: 4,
            };
            format.encode(&entry, entry.ino, index as i64 + 1, &mut records);
        }
        (records, names.iter().map(|name| name.len()).collect())
    }

    /// `records` as Linux leaves them in a buffer that held `Pages::FILL`:
    /// the padding after each name is not written.
    fn as_linux_writes(format: super::DirentFormat, records: &[u8], names: &[usize]) -> Vec<u8> {
        let mut bytes = records.to_vec();
        let mut at = 0;
        for &name_len in names {
            let reclen = format.record_len(name_len);
            let padding = at + format.name_offset() + name_len + 1;
            let end = match format {
                super::DirentFormat::Dirent64 => at + reclen,
                super::DirentFormat::Legacy => at + reclen - 1,
            };
            bytes[padding..end].fill(Pages::FILL);
            at += reclen;
        }
        bytes
    }

    const FORMATS: [super::DirentFormat; 2] =
        [super::DirentFormat::Dirent64, super::DirentFormat::Legacy];

    const TEN: [&[u8]; 10] = [b"a", b"b", b"c", b"d", b"e", b"f", b"g", b"h", b"i", b"j"];

    /// A buffer starting 16 bytes before a writable page, in a page the guest
    /// cannot write: Linux's first store, to the first record's `d_off`,
    /// fails, so the call fails and the writable page is left as it was.
    #[test]
    fn a_first_record_that_cannot_be_written_leaves_the_next_page_untouched() {
        for protection in [libc::PROT_NONE, libc::PROT_READ] {
            for format in FORMATS {
                let pages = Pages::new(&[protection, libc::PROT_READ | libc::PROT_WRITE]);
                let (records, names) = directory_records(format, &TEN);
                let copied = super::copy_records(
                    &mut LocalMemory::new(),
                    pages.address(PAGE - 16),
                    format,
                    &records,
                    &names,
                    0,
                );
                assert_eq!(copied, (0, Err(Errno::EFAULT)), "{format:?} {protection}");
                assert!(
                    pages.contents().iter().all(|&byte| byte == Pages::FILL),
                    "{format:?} after a page with protection {protection}: bytes changed"
                );
            }
        }
    }

    /// Guest memory whose every vectored copy fails with one error that is
    /// not a fault, as when the guest's thread is gone or the backend fails.
    struct FailingMemory(Errno);

    impl reverie::syscalls::MemoryAccess for FailingMemory {
        fn read_vectored(
            &self,
            _: &[std::io::IoSlice],
            _: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            Err(self.0)
        }

        fn write_vectored(
            &mut self,
            _: &[std::io::IoSlice],
            _: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            Err(self.0)
        }
    }

    /// Only a fault ends a copy of directory records short. Any other error
    /// of the copy is the call's error, as it was before directory streams,
    /// and not `EFAULT`: reading the records back, copying them out (through
    /// a buffer whose first store crosses a page, and through one where none
    /// does), and reading the prefix the `getdents64` observer hashes.
    #[test]
    fn a_copy_error_other_than_a_fault_is_returned() {
        let buf = |offset| reverie::syscalls::AddrMut::<u8>::from_raw(16 * PAGE + offset).unwrap();
        for errno in [Errno::ESRCH, Errno::EIO] {
            let mut bytes = [0; 32];
            assert_eq!(
                super::read_guest_prefix(&FailingMemory(errno), buf(0), &mut bytes),
                Err(errno)
            );
            for format in FORMATS {
                let (records, names) = directory_records(format, &TEN);
                for offset in [0, PAGE - 4] {
                    let copied = super::copy_records(
                        &mut FailingMemory(errno),
                        buf(offset),
                        format,
                        &records,
                        &names,
                        0,
                    );
                    assert_eq!(copied, (0, Err(errno)), "{format:?} at {offset}");
                }
                let read =
                    super::read_records(&FailingMemory(errno), buf(0), records.len(), format);
                assert_eq!(read.err(), Some(errno), "{format:?}");
            }
        }
    }

    /// A vectored copy that fails with `EFAULT`, which `process_vm_readv` and
    /// the KVM backend both return when the first byte is out of the guest's
    /// reach, copied nothing rather than failing: the caller decides what an
    /// empty copy means, as it does for a short one.
    #[test]
    fn a_fault_of_the_whole_copy_copies_nothing() {
        let buf = reverie::syscalls::AddrMut::<u8>::from_raw(16 * PAGE).unwrap();
        let mut bytes = [0; 32];
        assert_eq!(
            super::read_guest_prefix(&FailingMemory(Errno::EFAULT), buf, &mut bytes),
            Ok(0)
        );
        assert_eq!(
            super::write_guest_pieces(&mut FailingMemory(Errno::EFAULT), buf, &[(0, &bytes)]),
            Ok(0)
        );
    }

    /// A buffer starting 8 bytes before a writable page, in a page the guest
    /// cannot write. Linux's first store, the first record's position into
    /// its own `d_off`, lands in the writable page; its next, `d_ino`, fails.
    /// The call fails and leaves the position there.
    #[test]
    fn a_first_record_leaves_its_position_in_its_d_off() {
        for format in FORMATS {
            let pages = Pages::new(&[libc::PROT_READ, libc::PROT_READ | libc::PROT_WRITE]);
            let (records, names) = directory_records(format, &TEN);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 8),
                format,
                &records,
                &names,
                7,
            );
            assert_eq!(copied, (0, Err(Errno::EFAULT)), "{format:?}");
            let contents = pages.contents();
            assert_eq!(&contents[PAGE..PAGE + 8], &7i64.to_ne_bytes(), "{format:?}");
            assert!(
                contents[..PAGE]
                    .iter()
                    .chain(&contents[PAGE + 8..])
                    .all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes other than the first d_off changed"
            );
        }
    }

    /// A buffer starting 12 bytes before a writable page, in a page the guest
    /// can read but not write. The first record's `d_off` crosses into the
    /// writable page, and the CPU stores it at once, so Linux writes none of
    /// it: the part written in the writable page is put back.
    #[test]
    fn a_store_crossing_out_of_an_unwritable_page_writes_nothing() {
        for format in FORMATS {
            let pages = Pages::new(&[libc::PROT_READ, libc::PROT_READ | libc::PROT_WRITE]);
            let (records, names) = directory_records(format, &TEN);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 12),
                format,
                &records,
                &names,
                0,
            );
            assert_eq!(copied, (0, Err(Errno::EFAULT)), "{format:?}");
            assert!(
                pages.contents().iter().all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes changed"
            );
        }
    }

    /// The same store between two pages one of which the guest cannot write,
    /// for each other pair Detcore can leave as Linux does: one page it can
    /// read, or a write-only page before one it cannot touch. Whichever part
    /// is written first must be one Detcore can put back.
    #[test]
    fn a_store_crossing_pages_is_put_back_from_the_page_that_can_be_read() {
        let (none, read, write) = (libc::PROT_NONE, libc::PROT_READ, libc::PROT_WRITE);
        let both = read | write;
        for protections in [
            [read, write],
            [write, read],
            [write, none],
            [both, none],
            [both, read],
            [none, both],
        ] {
            for format in FORMATS {
                let pages = Pages::new(&protections);
                let (records, names) = directory_records(format, &TEN);
                let copied = super::copy_records(
                    &mut LocalMemory::new(),
                    pages.address(PAGE - 12),
                    format,
                    &records,
                    &names,
                    0,
                );
                assert_eq!(
                    copied,
                    (0, Err(Errno::EFAULT)),
                    "{protections:?} {format:?}"
                );
                assert!(
                    pages.contents().iter().all(|&byte| byte == Pages::FILL),
                    "{protections:?} {format:?}: bytes changed"
                );
            }
        }
    }

    /// Records of 24 bytes starting 100 bytes before a page the guest cannot
    /// write. Four fit. The fifth's `d_ino` crosses into that page, so Linux
    /// writes none of it: four records are returned, the fifth's position
    /// (the fourth's `d_off`) is written, and the four bytes before the page
    /// are left as they were.
    #[test]
    fn a_record_crossing_into_an_inaccessible_page_leaves_nothing_before_it() {
        for format in FORMATS {
            let pages = Pages::new(&[libc::PROT_READ | libc::PROT_WRITE, libc::PROT_NONE]);
            let (records, names) = directory_records(format, &TEN);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 100),
                format,
                &records,
                &names,
                0,
            );
            assert_eq!(copied, (4, Ok(96)), "{format:?}");
            let contents = pages.contents();
            assert_eq!(
                &contents[PAGE - 100..PAGE - 4],
                &as_linux_writes(format, &records[..96], &names[..4])[..],
                "{format:?}"
            );
            assert!(
                contents[..PAGE - 100]
                    .iter()
                    .chain(&contents[PAGE - 4..])
                    .all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes outside the records copied changed"
            );
        }
    }

    /// A read of a procfs file whose buffer faults two bytes in: a
    /// `seq_file` returns the two bytes, a sysctl fails with `EFAULT`, as
    /// measured on Linux 7.1; a file that fits copies whole either way.
    #[test]
    fn a_torn_procfs_read_is_partial_for_a_seq_file_and_a_fault_for_a_sysctl() {
        let pages = Pages::new(&[libc::PROT_READ | libc::PROT_WRITE, libc::PROT_NONE]);
        let edge = || Some(pages.address(PAGE - 2));
        let read = |bytes: &[u8], sysctl| {
            super::copy_procfs_read(&mut LocalMemory::new(), edge(), bytes, sysctl)
                .map_err(|error| error.into_errno().unwrap())
        };
        assert_eq!(read(b"Linux version\n", false), Ok(2));
        assert_eq!(
            read(b"65530\n", true),
            Err(reverie::syscalls::Errno::EFAULT)
        );
        assert_eq!(read(b"0\n", true), Ok(2));
        assert_eq!(read(b"", true), Ok(0));
    }

    /// A procfs read is EBADF unless the descriptor was opened for reading.
    #[test]
    fn only_a_descriptor_opened_for_reading_reads_procfs() {
        assert!(super::can_read(libc::O_RDONLY));
        assert!(super::can_read(libc::O_RDWR | libc::O_CLOEXEC));
        assert!(!super::can_read(libc::O_WRONLY));
        assert!(!super::can_read(libc::O_ACCMODE));
        assert!(!super::can_read(libc::O_PATH));
    }

    /// `kiocb_set_rw_flags` for a procfs file, in Linux 7.1's order.
    #[test]
    fn procfs_vector_flags_are_refused_in_linux_order() {
        use reverie::syscalls::Errno;
        const NOSIGNAL: i32 = 0x100;
        const UNKNOWN: i32 = 0x200;
        for accepted in [
            0,
            libc::RWF_HIPRI,
            libc::RWF_DSYNC,
            libc::RWF_SYNC,
            libc::RWF_APPEND,
            libc::RWF_NOAPPEND,
            NOSIGNAL,
        ] {
            assert_eq!(super::validate_procfs_vector_flags(accepted, true), Ok(()));
        }
        for (flags, errno) in [
            (libc::RWF_NOWAIT, Errno::EOPNOTSUPP),
            (libc::RWF_ATOMIC, Errno::EOPNOTSUPP),
            (libc::RWF_DONTCACHE, Errno::EOPNOTSUPP),
            (UNKNOWN, Errno::EOPNOTSUPP),
            (libc::RWF_APPEND | libc::RWF_NOAPPEND, Errno::EINVAL),
            (libc::RWF_NOWAIT | UNKNOWN, Errno::EOPNOTSUPP),
            (
                libc::RWF_APPEND | libc::RWF_NOAPPEND | UNKNOWN,
                Errno::EOPNOTSUPP,
            ),
            (
                libc::RWF_ATOMIC | libc::RWF_APPEND | libc::RWF_NOAPPEND,
                Errno::EINVAL,
            ),
            (
                libc::RWF_NOWAIT | libc::RWF_APPEND | libc::RWF_NOAPPEND,
                Errno::EINVAL,
            ),
        ] {
            assert_eq!(
                super::validate_procfs_vector_flags(flags, true),
                Err(errno),
                "{flags:#x}"
            );
        }
        // A file without `read_iter` (`/proc/modules`, `/proc/swaps`) takes
        // `RWF_HIPRI` alone, and refuses even the conflicting pair with
        // EOPNOTSUPP.
        assert_eq!(
            super::validate_procfs_vector_flags(libc::RWF_HIPRI, false),
            Ok(())
        );
        for flags in [
            libc::RWF_DSYNC,
            libc::RWF_APPEND,
            NOSIGNAL,
            libc::RWF_APPEND | libc::RWF_NOAPPEND,
        ] {
            assert_eq!(
                super::validate_procfs_vector_flags(flags, false),
                Err(Errno::EOPNOTSUPP),
                "{flags:#x}"
            );
        }
    }

    /// A buffer starting 4 bytes before a page the guest cannot write, in a
    /// page it can write but not read. Linux first writes the first record's
    /// `d_off`, 8 bytes in, which fails; it never writes the 4 bytes before
    /// the page.
    #[test]
    fn a_first_record_is_written_from_its_d_off() {
        for format in FORMATS {
            let pages = Pages::new(&[libc::PROT_WRITE, libc::PROT_NONE]);
            let (records, names) = directory_records(format, &TEN);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 4),
                format,
                &records,
                &names,
                0,
            );
            assert_eq!(copied, (0, Err(Errno::EFAULT)), "{format:?}");
            assert!(
                pages.contents().iter().all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes before the inaccessible page changed"
            );
        }
    }

    /// Records across two writable pages and into a third the guest cannot
    /// write, starting 40 bytes before the second page, so the third starts
    /// 4136 bytes in. Record 172 starts at 4128: Linux stores its position
    /// into record 171's `d_off` and its `d_ino`, which ends at the third
    /// page, and then fails at its `d_reclen`. The call returns 172 records
    /// and leaves that `d_ino` written.
    #[test]
    fn a_record_not_copied_is_left_as_linux_leaves_it() {
        let names: Vec<[u8; 1]> = (0..200u8).map(|i| [b'a' + i % 26]).collect();
        let names: Vec<&[u8]> = names.iter().map(|name| &name[..]).collect();
        for format in FORMATS {
            let rw = libc::PROT_READ | libc::PROT_WRITE;
            let pages = Pages::new(&[rw, rw, libc::PROT_NONE]);
            let start = PAGE - 40;
            let (records, lens) = directory_records(format, &names);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(start),
                format,
                &records,
                &lens,
                0,
            );
            assert_eq!(copied, (172, Ok(172 * 24)), "{format:?}");
            let contents = pages.contents();
            let mut expected = as_linux_writes(format, &records[..4128], &lens[..172]);
            expected.extend_from_slice(&records[4128..4136]);
            assert_eq!(&contents[start..start + 4136], &expected[..], "{format:?}");
            assert!(
                contents[..start]
                    .iter()
                    .chain(&contents[start + 4136..])
                    .all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes outside what Linux writes changed"
            );
        }
    }

    /// A second record with a 20-byte name, whose record ends past the end of
    /// what the guest can write. For `getdents64`, the NUL after the name is
    /// stored before the name and fails, so none of the name is written; for
    /// `getdents`, `d_type`, at the record's end, is stored before the name
    /// and fails. Either way the record's own `d_off` is not written: Linux
    /// stores it only with the next record or at the end of the call.
    #[test]
    fn a_name_is_stored_after_the_bytes_that_follow_it() {
        for format in FORMATS {
            let pages = Pages::new(&[libc::PROT_READ | libc::PROT_WRITE, libc::PROT_NONE]);
            let (records, names) = directory_records(format, &[b"a", b"abcdefghijklmnopqrst"]);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 50),
                format,
                &records,
                &names,
                0,
            );
            assert_eq!(copied, (1, Ok(24)), "{format:?}");
            let contents = &pages.contents()[PAGE - 50..PAGE];
            let mut expected = as_linux_writes(format, &records[..24], &names[..1]);
            expected.extend_from_slice(&records[24..32]);
            expected.extend_from_slice(&[Pages::FILL; 8]);
            expected.extend_from_slice(&records[40..42]);
            match format {
                super::DirentFormat::Dirent64 => expected.push(records[42]),
                super::DirentFormat::Legacy => expected.push(Pages::FILL),
            }
            expected.resize(50, Pages::FILL);
            assert_eq!(contents, &expected[..], "{format:?}");
        }
    }

    /// Records that all fit are all copied, apart from the padding after each
    /// name, which Linux does not write, and the bytes after them are left as
    /// they were.
    #[test]
    fn records_that_fit_are_copied_whole() {
        for format in FORMATS {
            let rw = libc::PROT_READ | libc::PROT_WRITE;
            let pages = Pages::new(&[rw, rw]);
            let (records, names) = directory_records(format, &TEN);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 50),
                format,
                &records,
                &names,
                0,
            );
            assert_eq!(copied, (10, Ok(240)), "{format:?}");
            let contents = pages.contents();
            assert_eq!(
                &contents[PAGE - 50..PAGE - 50 + records.len()],
                &as_linux_writes(format, &records, &names)[..],
                "{format:?}"
            );
            assert!(
                contents[..PAGE - 50]
                    .iter()
                    .chain(&contents[PAGE - 50 + records.len()..])
                    .all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes outside the records changed"
            );
        }
    }
}

/// `inject_fstat` and `add_fd` against a scripted guest whose "address space"
/// is this test process, so every injected syscall runs for real on host
/// memory and a real descriptor. Regression coverage for
/// <https://github.com/rrnewton/hermit/issues/3328>; the traced end-to-end case
/// is `tests_misc::tight_stack_openat`.
#[cfg(test)]
mod inject_fstat_scratch {
    use std::os::fd::IntoRawFd;
    use std::os::fd::RawFd;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Pid;
    use reverie::Tool;
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::ProtFlags;

    use super::*;
    use crate::Config;
    use crate::GlobalState;
    use crate::ThreadState;
    use crate::types::DetPid;

    /// Room for one `libc::stat`, 8-byte aligned like a real stack slot.
    const ARENA_WORDS: usize = 32;

    /// A guest stack scratch that is either writable or faults on commit,
    /// like the ptrace scratch below an `rsp` with no writable memory under
    /// it. The writable arena belongs to the guest and outlives every guard,
    /// so an early guard drop is reported by `guard_live` rather than by a
    /// write into freed memory.
    struct ScriptedStack {
        writable: bool,
        /// Match ptrace reserve/commit's actual zero write in the new controls.
        zero_on_commit: bool,
        arena: usize,
        capacity: usize,
        guard_live: Arc<AtomicBool>,
        /// A failing commit first overwrites the scratch's first word, as a
        /// commit that faults part way does.
        partial: bool,
        /// A page that a failing commit makes read-only after its partial
        /// write, so the borrower cannot put the bytes back.
        protect_on_failure: Option<usize>,
    }

    struct ScriptedStackGuard {
        guard_live: Arc<AtomicBool>,
    }

    impl Drop for ScriptedStackGuard {
        fn drop(&mut self) {
            self.guard_live.store(false, Ordering::SeqCst);
        }
    }

    impl reverie::Stack for ScriptedStack {
        type StackGuard = ScriptedStackGuard;

        fn size(&self) -> usize {
            panic!("inject_fstat must not query the scratch size")
        }
        fn capacity(&self) -> usize {
            panic!("inject_fstat must not query the scratch capacity")
        }
        fn push<'stack, T>(&mut self, _: T) -> Addr<'stack, T> {
            panic!("inject_fstat reserves its buffer rather than pushing one")
        }
        fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
            assert!(std::mem::size_of::<T>() <= self.capacity);
            AddrMut::from_raw(self.arena).unwrap()
        }
        fn commit(self) -> Result<Self::StackGuard, Errno> {
            if !self.writable {
                if self.partial {
                    // SAFETY: the arena (or scratch page) is this guest's,
                    // writable until protected below.
                    unsafe { (self.arena as *mut u64).write(0) };
                }
                if let Some(page) = self.protect_on_failure {
                    // SAFETY: the page is the test's own mapping.
                    assert_eq!(
                        unsafe { libc::mprotect(page as *mut _, 4096, libc::PROT_READ) },
                        0
                    );
                }
                return Err(Errno::EFAULT);
            }
            if self.zero_on_commit {
                // SAFETY: reserve checked the arena capacity and the owned
                // arena/page is writable for the duration of this stack.
                unsafe {
                    std::ptr::write_bytes(
                        self.arena as *mut u8,
                        0,
                        std::mem::size_of::<libc::stat>(),
                    )
                };
            }
            self.guard_live.store(true, Ordering::SeqCst);
            Ok(ScriptedStackGuard {
                guard_live: self.guard_live,
            })
        }
    }

    struct ScriptedGuest {
        config: Config,
        thread: ThreadState<()>,
        stack_writable: bool,
        zero_on_commit: bool,
        mmap_fails: bool,
        arena: Box<[u64; ARENA_WORDS]>,
        guard_live: Arc<AtomicBool>,
        injected: Vec<Sysno>,
        /// Whether a stack guard was live when each fstat was injected.
        fstat_guard_live: Vec<bool>,
        /// Buffer address of each injected fstat.
        fstat_buffers: Vec<usize>,
        /// (address, length) of each page the guest mapped.
        mapped: Vec<(usize, usize)>,
        /// (address, length) of each successful munmap.
        unmapped: Vec<(usize, usize)>,
        /// Descriptors closed through injection.
        closed: Vec<RawFd>,
        runtime_fd: Option<RawFd>,
        directory_batches: std::collections::VecDeque<Result<Vec<u8>, Errno>>,
        /// Scripted refusals of injected opens, in order; an open with none
        /// left opens a real file.
        open_errors: std::collections::VecDeque<Errno>,
        /// The paths of injected opens, as the guest's memory held them.
        opened_paths: Vec<Vec<u8>>,
        /// The real descriptors injected opens returned.
        opened_fds: Vec<RawFd>,
        /// Scripted errors of injected closes, in order, before any real
        /// close.
        close_errors: std::collections::VecDeque<Errno>,
        /// Every injected close's descriptor, a scripted failure included.
        close_attempts: Vec<RawFd>,
        /// The effective set an injected capget reports (the permitted set is
        /// every capability).
        capget_effective: u64,
        /// A page of its own for the stack scratch, in place of the arena.
        scratch_page: Option<usize>,
        /// A failing commit's partial write, and whether it then makes the
        /// scratch page read-only.
        commit_partial: bool,
        protect_page_on_commit_failure: bool,
        /// The injected syscall at which the scratch page becomes read-only,
        /// so that putting its bytes back fails.
        protect_page_on: Option<Sysno>,
        private_memory: Option<reverie::RuntimePrivateMemorySnapshot>,
        private_retired: bool,
        private_validations: std::sync::atomic::AtomicUsize,
        maps_reads: std::collections::VecDeque<Vec<u8>>,
    }

    impl ScriptedGuest {
        fn new(stack_writable: bool, mmap_fails: bool) -> (Detcore, Self) {
            let config = Config {
                virtualize_metadata: true,
                ..Config::default()
            };
            let pid = DetPid::from_raw(1);
            let mut thread = ThreadState::new(pid, &config, ());
            thread.detpid = Some(pid);
            let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
            let guest = Self {
                config,
                thread,
                stack_writable,
                zero_on_commit: false,
                mmap_fails,
                arena: Box::new([u64::MAX; ARENA_WORDS]),
                guard_live: Arc::new(AtomicBool::new(false)),
                injected: Vec::new(),
                fstat_guard_live: Vec::new(),
                fstat_buffers: Vec::new(),
                mapped: Vec::new(),
                unmapped: Vec::new(),
                closed: Vec::new(),
                runtime_fd: None,
                directory_batches: Default::default(),
                open_errors: Default::default(),
                opened_paths: Vec::new(),
                opened_fds: Vec::new(),
                close_errors: Default::default(),
                close_attempts: Vec::new(),
                capget_effective: (1 << 41) - 1,
                scratch_page: None,
                commit_partial: false,
                protect_page_on_commit_failure: false,
                protect_page_on: None,
                private_memory: None,
                private_retired: false,
                private_validations: std::sync::atomic::AtomicUsize::new(0),
                maps_reads: Default::default(),
            };
            (tool, guest)
        }

        /// A guest whose stack scratch is a page of its own, holding a
        /// pattern, and whose failure policy returns EOPNOTSUPP rather than
        /// stopping (as with --allow-unsupported-syscalls).
        fn with_scratch_page(stack_writable: bool) -> (Detcore, Self) {
            let (tool, mut guest) = Self::new(stack_writable, false);
            guest.config.panic_on_unsupported_syscalls = false;
            // SAFETY: a fresh private anonymous page, unmapped by the test.
            let page = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    4096,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(page, libc::MAP_FAILED);
            // SAFETY: the page is 4096 writable bytes.
            unsafe { std::ptr::write_bytes(page as *mut u8, 0xa5, 4096) };
            guest.scratch_page = Some(page as usize);
            (tool, guest)
        }

        /// The scratch page's bytes, and the page unmapped.
        fn take_scratch_page(&mut self) -> Vec<u8> {
            let page = self.scratch_page.take().unwrap();
            // SAFETY: the page is the test's mapping, readable throughout.
            let bytes = unsafe { std::slice::from_raw_parts(page as *const u8, 4096) }.to_vec();
            assert_eq!(unsafe { libc::munmap(page as *mut _, 4096) }, 0);
            bytes
        }
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for ScriptedGuest {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            panic!("fd registration must not send an RPC: {:?}", message.2)
        }
        fn config(&self) -> &Config {
            &self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for ScriptedGuest {
        type Memory = LocalMemory;
        type Stack = ScriptedStack;

        fn tid(&self) -> Pid {
            Pid::from_raw(1)
        }
        fn pid(&self) -> Pid {
            Pid::from_raw(1)
        }
        fn ppid(&self) -> Option<Pid> {
            None
        }
        fn is_runtime_owned_fd(&self, fd: RawFd) -> bool {
            self.runtime_fd == Some(fd)
        }
        fn runtime_private_memory_regions(&self) -> Option<reverie::RuntimePrivateMemory> {
            self.private_memory.clone().map(|facts| {
                if self.private_retired {
                    reverie::RuntimePrivateMemory::Unavailable
                } else {
                    reverie::RuntimePrivateMemory::Ready(facts)
                }
            })
        }
        fn runtime_private_memory_is_current(
            &self,
            snapshot: &reverie::RuntimePrivateMemorySnapshot,
        ) -> bool {
            self.private_validations.fetch_add(1, Ordering::SeqCst);
            !self.private_retired && self.private_memory.as_ref() == Some(snapshot)
        }
        fn memory(&self) -> Self::Memory {
            LocalMemory::new()
        }
        fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
            &mut self.thread
        }
        fn thread_state(&self) -> &ThreadState<()> {
            &self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("fd registration must not read registers")
        }
        async fn stack(&mut self) -> Self::Stack {
            ScriptedStack {
                writable: self.stack_writable,
                zero_on_commit: self.zero_on_commit,
                arena: self
                    .scratch_page
                    .unwrap_or(self.arena.as_mut_ptr() as usize),
                capacity: if self.scratch_page.is_some() {
                    4096
                } else {
                    ARENA_WORDS * std::mem::size_of::<u64>()
                },
                guard_live: self.guard_live.clone(),
                partial: self.commit_partial,
                protect_on_failure: self
                    .scratch_page
                    .filter(|_| self.protect_page_on_commit_failure),
            }
        }
        async fn daemonize(&mut self) {
            panic!("fd registration must not daemonize")
        }
        async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
            let (number, args) = syscall.into_parts();
            self.injected.push(number);
            if let Some(page) = self
                .scratch_page
                .filter(|_| self.protect_page_on == Some(number))
            {
                // SAFETY: the page is the test's own mapping.
                assert_eq!(
                    unsafe { libc::mprotect(page as *mut _, 4096, libc::PROT_READ) },
                    0
                );
            }
            if matches!(number, Sysno::getdents | Sysno::getdents64) {
                let bytes = self
                    .directory_batches
                    .pop_front()
                    .expect("scripted getdents")?;
                assert!(bytes.len() <= args.arg2);
                if !bytes.is_empty() {
                    let buffer = AddrMut::from_raw(args.arg1).unwrap();
                    self.memory().write_exact(buffer, &bytes)?;
                }
                return Ok(bytes.len() as i64);
            }
            // SAFETY: each arm runs the syscall Detcore asked for against this
            // process, on addresses Detcore obtained from this guest.
            let raw = match Syscall::from_raw(number, args) {
                Syscall::Lseek(call) if self.private_memory.is_some() => {
                    assert_eq!(call.offset(), 0);
                    assert_eq!(call.whence(), Whence::SEEK_SET);
                    0
                }
                Syscall::Read(call) if self.private_memory.is_some() => {
                    let bytes = self.maps_reads.pop_front().expect("scripted maps drain");
                    assert!(bytes.len() <= call.len());
                    self.memory().write_exact(call.buf().unwrap(), &bytes)?;
                    if bytes.is_empty() {
                        // The owner changes generation while the complete
                        // drain is in flight, before snapshot publication.
                        self.private_retired = true;
                    }
                    bytes.len() as i64
                }
                Syscall::Mmap(call) => {
                    assert!(call.addr().is_none(), "the kernel must choose the address");
                    assert_eq!(call.prot(), ProtFlags::PROT_READ | ProtFlags::PROT_WRITE);
                    assert_eq!(
                        call.flags(),
                        MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS
                    );
                    if self.mmap_fails {
                        return Err(Errno::ENOMEM);
                    }
                    let address = unsafe {
                        libc::mmap(
                            std::ptr::null_mut(),
                            call.len(),
                            libc::PROT_READ | libc::PROT_WRITE,
                            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                            -1,
                            0,
                        )
                    };
                    if address == libc::MAP_FAILED {
                        -1
                    } else {
                        self.mapped.push((address as usize, call.len()));
                        address as i64
                    }
                }
                Syscall::Fstat(call) => {
                    let buffer = call.stat().expect("fstat without a buffer").0.as_raw();
                    self.fstat_buffers.push(buffer);
                    self.fstat_guard_live
                        .push(self.guard_live.load(Ordering::SeqCst));
                    i64::from(unsafe { libc::fstat(call.fd(), buffer as *mut libc::stat) })
                }
                Syscall::Munmap(call) => {
                    let address = call.addr().expect("munmap without an address").as_raw();
                    let raw = i64::from(unsafe { libc::munmap(address as *mut _, call.len()) });
                    if raw == 0 {
                        self.unmapped.push((address, call.len()));
                    }
                    raw
                }
                Syscall::Close(call) => {
                    self.close_attempts.push(call.fd());
                    if let Some(errno) = self.close_errors.pop_front() {
                        return Err(errno);
                    }
                    self.closed.push(call.fd());
                    i64::from(unsafe { libc::close(call.fd()) })
                }
                Syscall::Openat(call) => {
                    let path = call.path().expect("openat without a path");
                    let path = path.read(&self.memory()).unwrap();
                    self.opened_paths
                        .push(path.as_os_str().as_encoded_bytes().to_vec());
                    if let Some(errno) = self.open_errors.pop_front() {
                        return Err(errno);
                    }
                    let (fd, _) = open_file();
                    self.opened_fds.push(fd);
                    i64::from(fd)
                }
                Syscall::Capget(call) => {
                    // Every capability, in the effective and permitted sets.
                    let data = call.data().expect("capget without data").as_raw();
                    let effective = self.capget_effective;
                    let words: [u32; 6] = [
                        effective as u32,
                        u32::MAX,
                        0,
                        (effective >> 32) as u32,
                        0x1ff,
                        0,
                    ];
                    let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_ne_bytes()).collect();
                    self.memory()
                        .write_exact(AddrMut::<u8>::from_raw(data).unwrap(), &bytes)?;
                    0
                }
                other => panic!("unexpected injected syscall {other:?}"),
            };
            Errno::result(raw)
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
            panic!("fd registration must not retire the guest")
        }
        fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("fd registration must not set a timer")
        }
        fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("fd registration must not set a timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("fd registration must not read a clock")
        }
    }

    pub(super) async fn raw_directory_projection() {
        use std::os::fd::AsRawFd;
        let directory = std::fs::File::open("/proc/self/fd").unwrap();
        for format in [DirentFormat::Legacy, DirentFormat::Dirent64] {
            let entries = [
                DirEntry {
                    ino: 91,
                    off: 29,
                    name: b"40960".to_vec(),
                    ty: 8,
                },
                DirEntry {
                    ino: 92,
                    off: 17,
                    name: b"1024".to_vec(),
                    ty: 10,
                },
                DirEntry {
                    ino: 93,
                    off: 31,
                    name: b"1023".to_vec(),
                    ty: 8,
                },
            ];
            let encode = |rows: &[DirEntry]| {
                let mut bytes = Vec::new();
                for entry in rows {
                    format.encode(entry, entry.ino, entry.off, &mut bytes);
                }
                bytes
            };
            let mut unmodified = encode(&entries[..1]);
            for at in format.name_offset() + entries[0].name.len() + 1..unmodified.len() {
                if at != format.type_offset(unmodified.len()) {
                    unmodified[at] = 0x7c;
                }
            }
            for (batches, expected, calls) in [
                (
                    vec![Ok(encode(&entries))],
                    Ok(encode(&[entries[0].clone(), entries[2].clone()])),
                    1,
                ),
                (
                    vec![Ok(encode(&entries[1..2])), Ok(encode(&entries[..1]))],
                    Ok(encode(&entries[..1])),
                    2,
                ),
                (
                    vec![Ok(encode(&entries[1..2])), Ok(Vec::new())],
                    Ok(Vec::new()),
                    2,
                ),
                (
                    vec![Ok(encode(&entries[1..2])), Err(Errno::EIO)],
                    Err(Errno::EIO),
                    2,
                ),
                (vec![Ok(unmodified.clone())], Ok(unmodified.clone()), 1),
            ] {
                let (_, mut guest) = ScriptedGuest::new(true, false);
                guest.config.virtualize_metadata = false;
                guest.config.backend = reverie::BackendCapabilities::DBT;
                guest.runtime_fd = Some(1024);
                let written = batches
                    .iter()
                    .filter_map(|batch| batch.as_ref().ok().map(Vec::len))
                    .max()
                    .unwrap();
                let untouched = expected.as_ref().is_ok_and(|bytes| bytes == &unmodified);
                guest.directory_batches = batches.into();
                let tool = <Detcore as Tool>::new(Pid::from_raw(1), &guest.config);
                let mut bytes = vec![0xa5; 512];
                let buffer: AddrMut<u8> = AddrMut::from_raw(bytes.as_mut_ptr() as usize).unwrap();
                let result = match format {
                    DirentFormat::Legacy => {
                        tool.handle_getdents(
                            &mut guest,
                            syscalls::Getdents::new()
                                .with_fd(directory.as_raw_fd() as u32)
                                .with_dirent(Some(buffer.cast()))
                                .with_count(bytes.len() as u32),
                        )
                        .await
                    }
                    DirentFormat::Dirent64 => {
                        tool.handle_getdents64(
                            &mut guest,
                            syscalls::Getdents64::new()
                                .with_fd(directory.as_raw_fd() as u32)
                                .with_dirent(Some(buffer.cast()))
                                .with_count(bytes.len() as u32),
                        )
                        .await
                    }
                };
                assert_eq!(guest.injected.len(), calls, "{format:?}");
                assert!(bytes[written..].iter().all(|byte| *byte == 0xa5));
                match expected {
                    Ok(expected) => {
                        assert_eq!(result.unwrap(), expected.len() as i64, "{format:?}");
                        assert_eq!(&bytes[..expected.len()], expected, "{format:?}");
                        if !untouched {
                            assert!(bytes[expected.len()..written].iter().all(|byte| *byte == 0));
                        }
                    }
                    Err(expected) => {
                        assert!(
                            matches!(result, Err(Error::Errno(actual)) if actual == expected),
                            "{format:?}: {result:?}"
                        );
                        assert!(bytes[..format.record_len(4)].iter().all(|byte| *byte == 0));
                    }
                }
            }
        }
    }

    #[test]
    fn private_maps_targets_require_complete_identity_or_translated_numbers() {
        use crate::procfs::BoundTask;
        let actual = Path::new("/proc/21/task/23/maps");
        for (numbers, expected) in [
            (
                BTreeMap::from([(21, 21), (23, 23)]),
                Some(BoundTask {
                    process: 21,
                    thread: Some(23),
                }),
            ),
            (
                BTreeMap::from([(21, 3), (23, 5)]),
                Some(BoundTask {
                    process: 3,
                    thread: Some(5),
                }),
            ),
            (BTreeMap::from([(21, 3)]), None),
        ] {
            assert_eq!(
                Detcore::<crate::record_or_replay::NoopTool>::maps_target_with_numbers(
                    actual,
                    Some(&numbers),
                    1,
                    1,
                ),
                expected
            );
        }
    }

    #[tokio::test]
    async fn private_maps_authenticates_the_opened_descriptor_in_its_task_view() {
        use std::os::fd::AsRawFd;
        let (_, mut guest) = ScriptedGuest::new(true, false);
        guest.config.backend = reverie::BackendCapabilities::DBT;
        guest.config.sequentialize_threads = true;
        guest.thread.physical_tid = Some(123456);
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &guest.config);
        assert!(!procfs_inode::guest_tids_name_host_tasks(
            &guest.config.backend,
            guest.thread.physical_tid,
        ));
        let file = std::fs::File::open("/proc/thread-self/maps").unwrap();
        let fd = file.as_raw_fd();
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0);
        let stat = DetStat::from(stat);
        let authenticated = tool.maps_descriptor_entry(&guest, &stat, fd).await;
        assert!(
            matches!(&authenticated, ProcfsEntry::At(path) if path.ends_with("maps")),
            "{authenticated:?}"
        );

        // The same spelling cannot authorize a different descriptor/object.
        let ordinary = tempfile::tempfile().unwrap();
        let mut ordinary_stat: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::fstat(ordinary.as_raw_fd(), &mut ordinary_stat) },
            0
        );
        assert!(matches!(
            tool.maps_descriptor_entry(&guest, &DetStat::from(ordinary_stat), ordinary.as_raw_fd())
                .await,
            ProcfsEntry::NotProcfs
        ));
        assert!(matches!(
            tool.maps_descriptor_entry(&guest, &stat, ordinary.as_raw_fd())
                .await,
            ProcfsEntry::Unknown
        ));
        guest.thread.changed_root.store(true, Ordering::SeqCst);
        assert!(matches!(
            tool.maps_descriptor_entry(&guest, &stat, fd).await,
            ProcfsEntry::Unknown
        ));
    }

    #[tokio::test]
    async fn private_maps_retired_during_drain_never_publish_a_snapshot() {
        let (_, mut guest) = ScriptedGuest::with_scratch_page(true);
        guest.config.sequentialize_threads = true;
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &guest.config);
        let target = crate::procfs::MapsTarget {
            task: crate::procfs::BoundTask {
                process: 2,
                thread: None,
            },
            incarnation: crate::procfs::TaskIncarnation {
                process: 1,
                thread: None,
            },
            mm: detcore_model::futex::MmId::initial(DetTid::from_raw(2)),
            private: reverie::RuntimePrivateMemorySnapshot {
                owner: [1, 2, 3, 4],
                version: 1,
                regions: vec![reverie::RuntimePrivateMemoryRegion {
                    start: 0x1000,
                    end: 0x2000,
                }],
            },
        };
        guest.private_memory = Some(target.private.clone());
        guest.maps_reads = [b"1000-2000 rw-p 00000000 00:00 0 \n".to_vec(), Vec::new()].into();
        let fd = 9;
        guest
            .thread
            .add_fd(fd, OFlag::O_RDONLY, FdType::Regular, None)
            .unwrap();
        let mut file = ProcfsFile::from_path(Path::new("/proc/2/maps")).unwrap();
        file.bind_maps_target(target.clone());
        guest
            .thread
            .with_detfd(fd, |detfd| detfd.set_procfs(file.clone()))
            .unwrap();
        let mut output = [0xa5_u8; 512];
        let call = syscalls::Read::new()
            .with_fd(fd)
            .with_buf(AddrMut::from_raw(output.as_mut_ptr() as usize))
            .with_len(output.len());

        let error = tool
            .initialize_procfs_snapshot(&mut guest, call)
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Tool(ref error) if error.to_string() ==
            "runtime-private maps view unavailable: target ownership changed during capture"),
            "{error:?}"
        );
        assert_eq!(guest.private_validations.load(Ordering::SeqCst), 2);
        assert!(guest.maps_reads.is_empty());
        assert_eq!(guest.injected, [Sysno::lseek, Sysno::read, Sysno::read]);
        assert_eq!(output, [0xa5; 512]);
        guest
            .thread
            .with_detfd(fd, |detfd| {
                assert!(detfd.procfs_needs_snapshot());
                assert_eq!(detfd.procfs_position(), Some((0, None)));
                assert_eq!(detfd.preview_procfs(512), None);
                assert_eq!(detfd.procfs_maps_target(), Some(target.clone()));
            })
            .unwrap();
        assert_eq!(guest.take_scratch_page(), vec![0xa5; 4096]);
    }

    /// A real descriptor the test owns by number, and its inode. Ownership is
    /// raw so a descriptor that Detcore closes is never closed a second time.
    fn open_file() -> (RawFd, u64) {
        let file = tempfile::tempfile().unwrap();
        let inode = file.metadata().unwrap().ino();
        (file.into_raw_fd(), inode)
    }

    fn close_unless_detcore_did(guest: &ScriptedGuest, fd: RawFd) {
        if !guest.closed.contains(&fd) {
            assert_eq!(unsafe { libc::close(fd) }, 0);
        }
    }

    fn recorded_inode(guest: &ScriptedGuest, fd: RawFd) -> Option<u64> {
        guest
            .thread
            .with_detfd(fd, |detfd| detfd.stat().map(|stat| stat.inode))
            .unwrap()
    }

    /// The errno of a call the failure policy answered, which a guest run
    /// with --allow-unsupported-syscalls sees.
    fn policy_errno(error: Error) -> Errno {
        error.into_errno().unwrap()
    }

    /// The task whose `/proc/3/auxv` the access tests open.
    const TARGET: crate::procfs::BoundTask = crate::procfs::BoundTask {
        process: 3,
        thread: None,
    };

    /// Another process's open of a rendered `/proc/<pid>/auxv`, decided
    /// through `ptrace_read_allowed` by the caller's probe opens, as the
    /// kernel beneath answers them:
    /// - the target's probe opens: allowed, one probe;
    /// - it is refused while the caller's `/proc/thread-self/stat` opens: the
    ///   refusal's errno;
    /// - both are refused with EACCES (the provider's backstop): the caller's
    ///   CAP_SYS_PTRACE decides, with and without it;
    /// - both fail with EMFILE (a full descriptor table): EMFILE, never the
    ///   fallback's EACCES;
    /// - an open Reverie reports did not run (ERESTARTSYS) is retried.
    #[tokio::test]
    async fn the_auxv_access_check_follows_the_kernel_beneaths_answers() {
        let paths = |guest: &ScriptedGuest| guest.opened_paths.clone();
        let target = b"/proc/3/auxv".to_vec();
        let serves = b"/proc/thread-self/stat".to_vec();
        let all = (1_u64 << 41) - 1;
        let without_ptrace = all & !(1 << 19);
        for (errors, effective, answer, probes, capget) in [
            (vec![], all, Ok(()), vec![target.clone()], false),
            (
                vec![Errno::EACCES],
                all,
                Err(Errno::EACCES),
                vec![target.clone(), serves.clone()],
                false,
            ),
            (
                vec![Errno::EACCES, Errno::EACCES],
                all,
                Ok(()),
                vec![target.clone(), serves.clone()],
                true,
            ),
            (
                vec![Errno::EACCES, Errno::EACCES],
                without_ptrace,
                Err(Errno::EACCES),
                vec![target.clone(), serves.clone()],
                true,
            ),
            (
                vec![Errno::EMFILE, Errno::EMFILE],
                without_ptrace,
                Err(Errno::EMFILE),
                vec![target.clone(), serves.clone()],
                false,
            ),
            (
                vec![Errno::ERESTARTSYS],
                without_ptrace,
                Ok(()),
                vec![target.clone(), target.clone()],
                false,
            ),
        ] {
            let (_tool, mut guest) = ScriptedGuest::with_scratch_page(true);
            guest.open_errors.extend(errors.clone());
            guest.capget_effective = effective;
            let result = Detcore::ptrace_read_allowed(&mut guest, TARGET).await;
            assert_eq!(result.unwrap(), answer, "{errors:?}");
            assert_eq!(paths(&guest), probes, "{errors:?}");
            assert_eq!(
                guest.injected.contains(&Sysno::capget),
                capget,
                "{errors:?}"
            );
            assert_eq!(guest.closed, guest.opened_fds, "every probe is closed");
            assert!(
                guest.take_scratch_page().iter().all(|byte| *byte == 0xa5),
                "{errors:?}: the scratch is left modified"
            );
        }
    }

    /// Guest stack scratch that cannot be used, or cannot be put back, is
    /// the failure policy's (EOPNOTSUPP under --allow-unsupported-syscalls,
    /// a named stop otherwise), never an access answer:
    /// - a commit that fails, its partial write put back: no probe runs,
    ///   and the scratch is as it was;
    /// - a commit that fails part way and whose partial write cannot be put
    ///   back;
    /// - a probe, and a capget (K1's), whose scratch becomes read-only
    ///   before it is put back.
    /// And a capget that succeeds puts its scratch back.
    #[tokio::test]
    async fn guest_stack_scratch_failures_go_to_the_failure_policy() {
        // A commit that fails, put back.
        let (_tool, mut guest) = ScriptedGuest::with_scratch_page(false);
        guest.commit_partial = true;
        let result = Detcore::ptrace_read_allowed(&mut guest, TARGET).await;
        assert_eq!(policy_errno(result.unwrap_err()), Errno::EOPNOTSUPP);
        assert!(guest.injected.is_empty());
        assert!(guest.take_scratch_page().iter().all(|byte| *byte == 0xa5));
        // A partial write that cannot be put back.
        let (_tool, mut guest) = ScriptedGuest::with_scratch_page(false);
        guest.commit_partial = true;
        guest.protect_page_on_commit_failure = true;
        let result = Detcore::ptrace_read_allowed(&mut guest, TARGET).await;
        assert_eq!(policy_errno(result.unwrap_err()), Errno::EOPNOTSUPP);
        assert!(guest.injected.is_empty());
        guest.take_scratch_page();
        let (_tool, mut guest) = ScriptedGuest::with_scratch_page(false);
        guest.commit_partial = true;
        guest.protect_page_on_commit_failure = true;
        let result =
            Detcore::inject_capget(&mut guest, rendered_procfs::CapabilitySet::Effective).await;
        assert_eq!(policy_errno(result.unwrap_err()), Errno::EOPNOTSUPP);
        guest.take_scratch_page();
        // A probe whose scratch cannot be put back: its descriptor is still
        // closed.
        let (_tool, mut guest) = ScriptedGuest::with_scratch_page(true);
        guest.protect_page_on = Some(Sysno::openat);
        let result = Detcore::ptrace_read_allowed(&mut guest, TARGET).await;
        assert_eq!(policy_errno(result.unwrap_err()), Errno::EOPNOTSUPP);
        assert_eq!(guest.closed, guest.opened_fds);
        guest.take_scratch_page();
        // A capget whose scratch cannot be put back.
        let (_tool, mut guest) = ScriptedGuest::with_scratch_page(true);
        guest.protect_page_on = Some(Sysno::capget);
        let result =
            Detcore::inject_capget(&mut guest, rendered_procfs::CapabilitySet::Effective).await;
        assert_eq!(policy_errno(result.unwrap_err()), Errno::EOPNOTSUPP);
        guest.take_scratch_page();
        // A capget that succeeds.
        let (_tool, mut guest) = ScriptedGuest::with_scratch_page(true);
        let result =
            Detcore::inject_capget(&mut guest, rendered_procfs::CapabilitySet::Effective).await;
        assert_eq!(result.unwrap(), Some((1 << 41) - 1));
        assert!(guest.take_scratch_page().iter().all(|byte| *byte == 0xa5));
    }

    /// The probe's close, as Linux's close allows: a close Reverie reports
    /// did not run (ERESTARTSYS) is retried; EINTR is not (Linux released
    /// the descriptor before returning it); any other error is the failure
    /// policy's, never a silent leak.
    #[tokio::test]
    async fn the_auxv_access_probe_closes_its_descriptor_as_linux_close_allows() {
        let (_tool, mut guest) = ScriptedGuest::with_scratch_page(true);
        guest.close_errors.push_back(Errno::ERESTARTSYS);
        let result = Detcore::ptrace_read_allowed(&mut guest, TARGET).await;
        assert_eq!(result.unwrap(), Ok(()));
        let [fd] = guest.opened_fds[..] else { panic!() };
        assert_eq!(
            guest.close_attempts,
            [fd, fd],
            "a close that did not run is retried"
        );
        assert_eq!(guest.closed, [fd]);
        guest.take_scratch_page();
        for (errno, failed) in [(Errno::EINTR, false), (Errno::EBADF, true)] {
            let (_tool, mut guest) = ScriptedGuest::with_scratch_page(true);
            guest.close_errors.push_back(errno);
            let result = Detcore::ptrace_read_allowed(&mut guest, TARGET).await;
            let [fd] = guest.opened_fds[..] else { panic!() };
            assert_eq!(guest.close_attempts, [fd], "{errno} is not retried");
            match result {
                Ok(answer) => {
                    assert!(!failed, "{errno}");
                    assert_eq!(answer, Ok(()));
                }
                Err(error) => {
                    assert!(failed, "{errno}");
                    assert_eq!(policy_errno(error), Errno::EOPNOTSUPP);
                }
            }
            // The scripted error left the real descriptor open.
            assert_eq!(unsafe { libc::close(fd) }, 0);
            assert!(guest.take_scratch_page().iter().all(|byte| *byte == 0xa5));
        }
    }

    /// Exercise the actual shared helper with nonzero borrowed bytes and
    /// real kernel fstat success/error. The selectable zero commit matches
    /// reverie-ptrace GuestStack::commit, rather than duplicating Detcore's
    /// restoration algorithm in a standalone test helper.
    #[tokio::test]
    async fn injected_fstat_restores_nonzero_borrowed_bytes_on_success_and_error() {
        for success in [true, false] {
            let (_, mut guest) = ScriptedGuest::new(true, false);
            guest.zero_on_commit = true;
            for (index, word) in guest.arena.iter_mut().enumerate() {
                *word = 0xa5a5_a5a5_a5a5_a5a5 ^ index as u64;
            }
            let original = *guest.arena;
            let (fd, inode) = if success { open_file() } else { (-1, 0) };
            let copied = Detcore::inject_stat_on_stack(&mut guest, |statptr| {
                Syscall::Fstat(syscalls::Fstat::new().with_fd(fd).with_stat(Some(statptr)))
            })
            .await;
            if success {
                assert_eq!(unsafe { libc::close(fd) }, 0);
                assert_eq!(copied.unwrap().st_ino, inode);
            } else {
                assert_eq!(copied.err(), Some(Errno::EBADF));
            }
            assert_eq!(
                *guest.arena, original,
                "success={success}: all borrowed and neighboring bytes"
            );
            assert_eq!(guest.injected, [Sysno::fstat]);
            assert_eq!(guest.fstat_guard_live, [true]);
            assert!(!guest.guard_live.load(Ordering::SeqCst));
            assert!(guest.mapped.is_empty());
        }
    }

    #[tokio::test]
    async fn partial_fstat_commit_is_restored_before_the_real_page_fallback() {
        let (fd, inode) = open_file();
        let (tool, mut guest) = ScriptedGuest::new(false, false);
        guest.commit_partial = true;
        let original = *guest.arena;
        let result = tool.inject_fstat(&mut guest, fd).await;
        assert_eq!(unsafe { libc::close(fd) }, 0);
        assert_eq!(result.unwrap().st_ino, inode);
        assert_eq!(
            *guest.arena, original,
            "the failed commit's partial zero write must be put back"
        );
        assert_eq!(guest.injected, [Sysno::mmap, Sysno::fstat, Sysno::munmap]);
        assert_eq!(
            guest.fstat_guard_live,
            [false],
            "the failed stack guard was never constructed"
        );
        assert_eq!(
            guest.mapped, guest.unmapped,
            "the actual temporary page was removed"
        );
        assert!(!guest.guard_live.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn writable_stack_scratch_is_used_while_its_guard_is_live() {
        let (fd, inode) = open_file();
        let (tool, mut guest) = ScriptedGuest::new(true, false);

        let result = tool
            .add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await;
        close_unless_detcore_did(&guest, fd);

        assert_eq!(result, Ok(()));
        assert_eq!(guest.injected, [Sysno::fstat]);
        assert_eq!(
            guest.fstat_guard_live,
            [true],
            "the stack guard must outlive the injected fstat: backends whose \
             scratch is an arena free it when the guard drops"
        );
        assert_eq!(
            guest.fstat_buffers,
            [guest.arena.as_ptr() as usize],
            "fstat must write into the stack scratch"
        );
        assert_eq!(
            *guest.arena,
            [u64::MAX; ARENA_WORDS],
            "the original borrowed bytes, including neighbors, must remain unchanged"
        );
        assert_eq!(recorded_inode(&guest, fd), Some(inode));
    }

    #[tokio::test]
    async fn faulting_stack_scratch_falls_back_to_a_transient_page() {
        let (fd, inode) = open_file();
        let (tool, mut guest) = ScriptedGuest::new(false, false);

        let result = tool
            .add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await;
        close_unless_detcore_did(&guest, fd);

        assert_eq!(
            result,
            Ok(()),
            "a stack that cannot hold the fstat buffer must not fail the open"
        );
        assert_eq!(guest.injected, [Sysno::mmap, Sysno::fstat, Sysno::munmap]);
        let [(page, len)] = guest.mapped[..] else {
            panic!(
                "expected exactly one transient page, got {:?}",
                guest.mapped
            );
        };
        assert_eq!(
            guest.fstat_buffers,
            [page],
            "fstat must write into the transient page"
        );
        assert_eq!(
            guest.unmapped,
            [(page, len)],
            "the transient page must be unmapped, whole"
        );
        assert_eq!(recorded_inode(&guest, fd), Some(inode));
    }

    #[tokio::test]
    async fn descriptor_is_closed_when_no_scratch_can_be_found() {
        let (fd, _) = open_file();
        let (tool, mut guest) = ScriptedGuest::new(false, true);

        let result = tool
            .add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await;
        close_unless_detcore_did(&guest, fd);

        assert_eq!(result, Err(Errno::ENOMEM));
        assert_eq!(guest.injected, [Sysno::mmap, Sysno::close]);
        assert_eq!(
            guest.closed,
            [fd],
            "the descriptor must not stay open behind the error"
        );
        assert_eq!(
            guest.thread.with_detfd(fd, |_| ()),
            Err(Errno::EBADF),
            "a descriptor that failed registration must not be modeled"
        );
    }

    /// The canonical statfs view: fixed caps on the free counts, clamped to
    /// the mount's totals, and a zero `f_fsid`, with the totals kept. These
    /// fabricated-input cases moved here from the KVM executor's copy of
    /// this canonicalization, deleted in favour of this one.
    #[test]
    fn canonicalize_statfs_buf_caps_free_counts_and_zeroes_the_fsid() {
        let (tool, mut guest) = ScriptedGuest::new(false, false);
        let mut canonical = |mut statfs: libc::statfs| {
            let buffer = AddrMut::from_raw(std::ptr::from_mut(&mut statfs) as usize);
            tool.canonicalize_statfs_buf(&mut guest, buffer).unwrap();
            statfs
        };
        // SAFETY: fsid_t is two initialized integer words on x86-64.
        let fsid = |statfs: &libc::statfs| unsafe {
            std::mem::transmute::<libc::fsid_t, [i32; 2]>(statfs.f_fsid)
        };
        // SAFETY: zero is a valid statfs.
        let zero = unsafe { std::mem::zeroed::<libc::statfs>() };

        // Totals above both caps: the free counts take the caps.
        let mut large = zero;
        large.f_blocks = 10_000_000;
        large.f_bfree = 7_123_456;
        large.f_bavail = 6_987_654;
        large.f_files = 2_000_000;
        large.f_ffree = 1_234_567;
        // SAFETY: fsid_t is two integer words on x86-64.
        large.f_fsid = unsafe { std::mem::transmute::<[i32; 2], libc::fsid_t>([0x1234, 0x5678]) };
        let large = canonical(large);
        assert_eq!(
            (large.f_bfree, large.f_bavail, large.f_ffree),
            (1_000_000, 1_000_000, 500_000)
        );
        assert_eq!(fsid(&large), [0, 0]);
        assert_eq!((large.f_blocks, large.f_files), (10_000_000, 2_000_000));

        // Totals under the caps: the free counts are the totals, never more.
        let mut small = zero;
        small.f_blocks = 42;
        small.f_bfree = 40;
        small.f_files = 300;
        small.f_ffree = 7;
        let small = canonical(small);
        assert_eq!(
            (small.f_bfree, small.f_bavail, small.f_ffree),
            (42, 42, 300)
        );
        assert_eq!((small.f_blocks, small.f_files), (42, 300));

        // A filesystem that keeps no inode count reports no free inodes, and
        // its free blocks are still clamped to its total.
        let mut no_inodes = zero;
        no_inodes.f_blocks = 42;
        no_inodes.f_ffree = 9;
        let no_inodes = canonical(no_inodes);
        assert_eq!((no_inodes.f_files, no_inodes.f_ffree), (0, 0));
        assert_eq!((no_inodes.f_bfree, no_inodes.f_bavail), (42, 42));

        // A null buffer is left alone.
        tool.canonicalize_statfs_buf(&mut guest, None).unwrap();
    }
}

#[cfg(test)]
mod rebound_path_tests {
    use std::path::PathBuf;

    use super::rebound_path;

    /// A namespace change's operand is reported as the path it names, made
    /// absolute. One that could not be read (a tracer may not read a
    /// non-dumpable guest's memory) or made absolute is reported as `/`, the
    /// ancestor of every path, so that no host input change is named in that
    /// run instead of one the guest may have made.
    #[test]
    fn an_operand_that_cannot_be_established_is_reported_as_the_root() {
        assert_eq!(rebound_path(Some(PathBuf::from("/data/F"))), "/data/F");
        assert_eq!(rebound_path(None), "/");
        assert_eq!(rebound_path(Some(PathBuf::from("F"))), "/");
    }
}

#[cfg(test)]
mod untraced_code_tests {
    use std::path::Path;
    use std::path::PathBuf;

    use nix::fcntl::OFlag;
    use reverie::syscalls::Errno;
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::DescriptorFacts;
    use super::DetPid;
    use super::PROC_ROOT_INO;
    use super::PidfdSignalScope;
    use super::ProcessDirectoryDescriptor;
    use super::classify_process_directory;
    use super::descriptor_identity;
    use super::may_change_untraced_code;
    use super::may_write_process_memory;
    use super::pidfd_signal_scope;

    const PAGE: (u64, u64) = (0x7100_0000, 0x7100_1000);

    fn args(arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> SyscallArgs {
        SyscallArgs::new(
            arg0 as usize,
            arg1 as usize,
            arg2 as usize,
            arg3 as usize,
            arg4 as usize,
            0,
        )
    }

    /// A thread cloned without CLONE_FILES has its own descriptor table, so
    /// a descriptor is read through the thread that opened it. Through its
    /// leader the same number names nothing here, which would make an
    /// ordinary write-mode open count as unreadable (a false `/` that
    /// withholds a real host input change) and would miss the thread's own
    /// process-memory file.
    #[test]
    fn a_descriptor_is_read_through_the_thread_that_opened_it() {
        use std::os::fd::AsRawFd;

        let directory = tempfile::tempdir().unwrap();
        let ordinary = directory.path().join("out");
        let named_mem = directory.path().join("mem");
        std::thread::spawn(move || {
            // SAFETY: gives only this thread a private copy of the table.
            assert_eq!(unsafe { libc::unshare(libc::CLONE_FILES) }, 0);
            // SAFETY: plain syscalls on this thread's own descriptors.
            let tid = unsafe { libc::gettid() };
            let leader = std::process::id() as i32;
            let files = [
                (std::fs::File::create(&ordinary).unwrap(), false),
                (std::fs::File::create(&named_mem).unwrap(), false),
                (
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open("/proc/thread-self/mem")
                        .unwrap(),
                    true,
                ),
            ];
            for (number, (file, is_process_memory)) in (900..).zip(files) {
                assert_eq!(unsafe { libc::dup2(file.as_raw_fd(), number) }, number);
                drop(file);
                let identity = descriptor_identity(tid, number);
                assert!(identity.is_some(), "{number}");
                assert_eq!(descriptor_identity(leader, number), None, "{number}");
                assert_eq!(
                    may_write_process_memory(
                        OFlag::O_WRONLY,
                        identity
                            .as_ref()
                            .map(|(link, procfs)| (link.as_path(), *procfs)),
                    ),
                    is_process_memory,
                    "{identity:?}"
                );
                assert_eq!(unsafe { libc::close(number) }, 0);
            }
        })
        .join()
        .unwrap();
    }

    /// Every call that can replace, unprotect, discard or unmap the range
    /// counts when its interval reaches any byte of it, and not when it stops
    /// short of it or starts after it.
    #[test]
    fn a_call_counts_exactly_when_it_may_change_the_range() {
        let fixed = libc::MAP_FIXED as u64 | libc::MAP_PRIVATE as u64;
        let huge = fixed | libc::MAP_HUGETLB as u64;
        let huge_1gb = (30 << libc::MAP_HUGE_SHIFT) as u64;
        let remap_fixed = (libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED) as u64;
        let remap = libc::SHM_REMAP as u64;
        let cases = [
            (
                "mmap fixed over it",
                Sysno::mmap,
                args(0x7100_0000, 0x1000, 7, fixed, 0),
                true,
            ),
            (
                "mmap fixed reaching it",
                Sysno::mmap,
                args(0x70ff_f000, 0x1001, 7, fixed, 0),
                true,
            ),
            (
                "mmap fixed below it",
                Sysno::mmap,
                args(0x70ff_f000, 0x1000, 7, fixed, 0),
                false,
            ),
            (
                "mmap fixed above it",
                Sysno::mmap,
                args(0x7100_1000, 0x1000, 7, fixed, 0),
                false,
            ),
            (
                "mmap hinted at it",
                Sysno::mmap,
                args(0x7100_0000, 0x1000, 7, 0x22, 0),
                false,
            ),
            // The host default here is 2 MiB; 0x7100_0000 is 2 MiB aligned.
            (
                "mmap fixed default huge page holding it",
                Sysno::mmap,
                args(0x7110_0000, 1, 7, huge, 0),
                true,
            ),
            (
                "mmap fixed small page there",
                Sysno::mmap,
                args(0x7110_0000, 1, 7, fixed, 0),
                false,
            ),
            (
                "mmap fixed default huge page below it",
                Sysno::mmap,
                args(0x70ff_f000, 1, 7, huge, 0),
                false,
            ),
            (
                "mmap fixed 1 GiB page holding it",
                Sysno::mmap,
                args(0x4000_0000, 1, 7, huge | huge_1gb, 0),
                true,
            ),
            (
                "mmap fixed 1 GiB page elsewhere",
                Sysno::mmap,
                args(0x8000_0000, 1, 7, huge | huge_1gb, 0),
                false,
            ),
            (
                "mprotect of it",
                Sysno::mprotect,
                args(0x7100_0000, 0x1000, 7, 0, 0),
                true,
            ),
            (
                "mprotect covering it",
                Sysno::mprotect,
                args(0x7000_0000, 0x200_0000, 7, 0, 0),
                true,
            ),
            (
                "mprotect of a zero length",
                Sysno::mprotect,
                args(0x7100_0000, 0, 7, 0, 0),
                true,
            ),
            (
                "pkey_mprotect of it",
                Sysno::pkey_mprotect,
                args(0x7100_0000, 1, 7, 0, 0),
                true,
            ),
            (
                "madvise of it",
                Sysno::madvise,
                args(0x7100_0000, 0x1000, 4, 0, 0),
                true,
            ),
            (
                "munmap of it",
                Sysno::munmap,
                args(0x7100_0000, 0x1000, 0, 0, 0),
                true,
            ),
            (
                "munmap past it",
                Sysno::munmap,
                args(0x7100_1000, 0x1000, 0, 0, 0),
                false,
            ),
            (
                "mremap from it",
                Sysno::mremap,
                args(0x7100_0000, 0x1000, 0x2000, 1, 0),
                true,
            ),
            (
                "mremap fixed onto it",
                Sysno::mremap,
                args(0x6000_0000, 0x1000, 0x1000, remap_fixed, 0x7100_0000),
                true,
            ),
            (
                "mremap fixed of a small page into its gigabyte",
                Sysno::mremap,
                args(0x6000_0000, 0x1000, 0x1000, remap_fixed, 0x4000_0000),
                false,
            ),
            (
                "mremap moving elsewhere",
                Sysno::mremap,
                args(0x6000_0000, 0x1000, 0x2000, 1, 0x7100_0000),
                false,
            ),
            (
                "shmat remapping at it",
                Sysno::shmat,
                args(3, 0x7100_0000, remap, 0, 0),
                true,
            ),
            (
                "shmat remapping below it",
                Sysno::shmat,
                args(3, 0x6000_0000, remap, 0, 0),
                true,
            ),
            (
                "shmat without remap",
                Sysno::shmat,
                args(3, 0x7100_0000, 0, 0, 0),
                false,
            ),
            (
                "shmat anywhere",
                Sysno::shmat,
                args(3, 0, remap, 0, 0),
                false,
            ),
            ("brk", Sysno::brk, args(0x7100_0000, 0, 0, 0, 0), false),
            // A scalar argument that equals an address in the range is not one.
            (
                "lseek to that offset",
                Sysno::lseek,
                args(3, 0x7100_0000, 0, 0, 0),
                false,
            ),
            (
                "an address near the end",
                Sysno::munmap,
                args(u64::MAX - 0xfff, 0x1000, 0, 0, 0),
                false,
            ),
        ];
        for (label, number, args, expected) in cases {
            assert_eq!(
                may_change_untraced_code(number, &args, PAGE, 2 << 20),
                expected,
                "{label}"
            );
        }
    }

    /// `pidfd_send_signal`'s flags select one scope, as Linux's do; an unknown
    /// bit or two scopes are EINVAL (https://github.com/rrnewton/hermit/issues/4108).
    #[test]
    fn pidfd_signal_flags_select_one_scope() {
        assert_eq!(pidfd_signal_scope(0), Ok(PidfdSignalScope::Process));
        assert_eq!(pidfd_signal_scope(1), Ok(PidfdSignalScope::Thread));
        assert_eq!(pidfd_signal_scope(2), Ok(PidfdSignalScope::Process));
        assert_eq!(pidfd_signal_scope(4), Ok(PidfdSignalScope::ProcessGroup));
        for flags in [3, 5, 6, 7, 8, 9, 1 << 31, u32::MAX] {
            assert_eq!(pidfd_signal_scope(flags), Err(Errno::EINVAL), "{flags:#x}");
        }
    }

    /// A descriptor names a process for `pidfd_send_signal` only when it is a
    /// directory whose parent is the root of the guest's procfs and whose name
    /// is a pid; another file is EBADF, and a pidfd Detcore did not open, an
    /// unreadable descriptor or another procfs instance cannot be classified.
    #[test]
    fn a_proc_process_directory_names_its_process() {
        use ProcessDirectoryDescriptor as D;
        let procfs = |link: &str, parent: Option<(u64, u64)>| DescriptorFacts {
            link: PathBuf::from(link),
            on_procfs: true,
            is_dir: parent.is_some(),
            dev: 22,
            parent,
            guest_proc_dev: Some(22),
        };
        let reaped = |link: &str| DescriptorFacts {
            is_dir: true,
            ..procfs(link, None)
        };
        let root = Some((22, PROC_ROOT_INO));
        let cases = [
            (
                "process",
                procfs("/proc/3", root),
                D::Process(DetPid::from_raw(3)),
            ),
            (
                "reaped process",
                reaped("/proc/7 (deleted)"),
                D::Process(DetPid::from_raw(7)),
            ),
            (
                "reaped task directory",
                reaped("/proc/7/task/8 (deleted)"),
                D::NotAProcess,
            ),
            (
                "reaped elsewhere",
                reaped("/mnt/p/7 (deleted)"),
                D::Unknown("it names a reaped task outside the guest's /proc"),
            ),
            (
                "procfs mounted elsewhere",
                procfs("/mnt/p/3", root),
                D::Process(DetPid::from_raw(3)),
            ),
            (
                "task directory",
                procfs("/proc/3/task/3", Some((22, 4000))),
                D::NotAProcess,
            ),
            (
                "procfs file",
                procfs("/proc/3/status", None),
                D::NotAProcess,
            ),
            (
                "named root entry",
                procfs("/proc/sys", root),
                D::NotAProcess,
            ),
            ("pid zero", procfs("/proc/0", root), D::NotAProcess),
            ("leading zero", procfs("/proc/03", root), D::NotAProcess),
            ("signed", procfs("/proc/+3", root), D::NotAProcess),
            (
                "another procfs root",
                procfs("/proc/3", Some((23, PROC_ROOT_INO))),
                D::NotAProcess,
            ),
            (
                "not the guest's procfs",
                DescriptorFacts {
                    guest_proc_dev: Some(99),
                    ..procfs("/proc/3", root)
                },
                D::Unknown("it is a procfs that is not the guest's /proc"),
            ),
            (
                "ordinary directory",
                DescriptorFacts {
                    on_procfs: false,
                    ..procfs("/tmp/3", root)
                },
                D::NotAProcess,
            ),
            (
                "pidfd",
                DescriptorFacts {
                    on_procfs: false,
                    ..procfs("anon_inode:[pidfd]", None)
                },
                D::Unknown("it is a pidfd that Detcore did not open with pidfd_open"),
            ),
            (
                "pidfs pidfd",
                DescriptorFacts {
                    on_procfs: false,
                    ..procfs("pidfd:[1234]", None)
                },
                D::Unknown("it is a pidfd that Detcore did not open with pidfd_open"),
            ),
        ];
        for (label, facts, expected) in cases {
            assert_eq!(
                classify_process_directory(Some(&facts)),
                expected,
                "{label}"
            );
        }
        assert_eq!(
            classify_process_directory(None),
            D::Unknown("Hermit cannot read what the descriptor names")
        );
    }

    /// A write-mode open counts when its descriptor is a procfs file named
    /// `mem`, or cannot be read at all; a read-only or `O_PATH` open, an
    /// ordinary file named `mem`, a pipe, and another procfs file do not.
    #[test]
    fn an_open_counts_when_it_may_write_process_memory() {
        let proc_mem = Some((Path::new("/proc/12/mem"), true));
        let cases = [
            ("process mem read-write", OFlag::O_RDWR, proc_mem, true),
            (
                "task mem write-only",
                OFlag::O_WRONLY,
                Some((Path::new("/proc/12/task/13/mem"), true)),
                true,
            ),
            ("an unreadable descriptor", OFlag::O_WRONLY, None, true),
            ("process mem read-only", OFlag::O_RDONLY, proc_mem, false),
            (
                "process mem as a path",
                OFlag::O_PATH | OFlag::O_RDWR,
                proc_mem,
                false,
            ),
            (
                "an ordinary file named mem",
                OFlag::O_WRONLY | OFlag::O_CREAT,
                Some((Path::new("/tmp/mem"), false)),
                false,
            ),
            (
                "a name that leads to a pipe",
                OFlag::O_WRONLY,
                Some((Path::new("pipe:[4026]"), false)),
                false,
            ),
            (
                "another procfs file",
                OFlag::O_WRONLY,
                Some((Path::new("/proc/12/attr/current"), true)),
                false,
            ),
            (
                "an unreadable read-only descriptor",
                OFlag::O_RDONLY,
                None,
                false,
            ),
        ];
        for (label, flags, descriptor, expected) in cases {
            assert_eq!(
                may_write_process_memory(flags, descriptor),
                expected,
                "{label}"
            );
        }
    }
}

/// Closing advice of every flock refusal. It names no backend: LOCK_NB is the
/// form Detcore can serve wherever the backend supports flock, and ENOLCK
/// reaches the guest only when the run opted in to unsupported syscalls.
const FLOCK_REFUSAL_ADVICE: &str = "LOCK_NB is the supported form where the backend \
     supports flock at all; ENOLCK is returned instead only under \
     --allow-unsupported-syscalls.";

/// Report a flock refusal in every disposition, fail-closed (the run ends)
/// and opted in (the guest gets ENOLCK): through tracing, and directly on
/// stderr so that neither --log=off nor --log-file hides the reason.
fn report_flock_refusal(message: &str) {
    error!("{message}");
    let _ = std::io::Write::write_fmt(
        &mut crate::util::RetryingStderr,
        format_args!("hermit: {message}\n"),
    );
}

#[cfg(test)]
mod linux_uapi_statx_tests {
    use super::statx_arguments_valid;

    #[test]
    fn statx_sync_modes_keep_linux_positive_and_negative_contract() {
        for sync in [0, 0x2000, 0x4000] {
            assert!(statx_arguments_valid(sync, 0, false));
        }
        assert!(!statx_arguments_valid(0x6000, 0, false));
        assert!(!statx_arguments_valid(0x6000, 0, true));
        assert!(!statx_arguments_valid(0, 0x8000_0000, true));
        assert!(!statx_arguments_valid(0x4000_0000, 0, false));
        // Preserve the existing descriptor/no-path rule for other flag bits.
        assert!(statx_arguments_valid(0x4000_0000, 0, true));
    }
}
