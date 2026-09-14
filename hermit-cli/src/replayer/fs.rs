/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::fs::FileExt;

use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Fcntl;
use reverie::syscalls::FcntlCmd;
use reverie::syscalls::Ftruncate;
use reverie::syscalls::Getdents;
use reverie::syscalls::Getdents64;
use reverie::syscalls::Getuid;
use reverie::syscalls::Ioctl;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Pread64;
use reverie::syscalls::Read;
use reverie::syscalls::Readlink;
use reverie::syscalls::RtSigtimedwait;
use reverie::syscalls::RtTgsigqueueinfo;
use reverie::syscalls::Statx;
use reverie::syscalls::Syscall;
use reverie::syscalls::Timespec;
use reverie::syscalls::family::StatFamily;
use reverie::syscalls::family::WriteFamily;
use reverie::syscalls::ioctl;

use super::Replayer;
use crate::event::FileCloneImage;
use crate::event::ReplayFdKind;
use crate::event::deterministic_ioctl_error;
use crate::vectored_offset;

#[repr(C)]
struct UserSignalInfoHead {
    signo: libc::c_int,
    errno: libc::c_int,
    code: libc::c_int,
    padding: libc::c_int,
    pid: libc::pid_t,
    uid: libc::uid_t,
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-696): Review lossless replay-output backpressure handling.
async fn wait_for_replay_output(output_fd: RawFd) -> std::io::Result<bool> {
    // F_DUPFD_CLOEXEC keeps the endpoint alive while the bounded blocking task
    // polls it, without changing the shared open-file-description flags.
    let duplicate = unsafe { libc::fcntl(output_fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned by this task.
    let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };
    let readiness = tokio::task::spawn_blocking(move || {
        let mut pollfd = libc::pollfd {
            fd: duplicate.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // A finite timeout keeps a cancelled replay from leaving an unbounded
        // blocking-pool task behind. Timeout or EINTR asks the caller to retry
        // its nonblocking write in a new bounded task.
        let ready = unsafe { libc::poll(&mut pollfd, 1, 100) };
        if ready > 0 {
            return Ok(pollfd.revents & libc::POLLOUT != 0);
        }
        if ready == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            Ok(true)
        } else {
            Err(error)
        }
    })
    .await;
    match readiness {
        Ok(result) => result,
        Err(error) => Err(std::io::Error::other(format!(
            "replay output readiness task failed: {error}"
        ))),
    }
}

const CLONE_COPY_CHUNK_BYTES: usize = 1024 * 1024;

fn restore_sparse_clone_sidecar(
    source: &std::fs::File,
    destination: &std::fs::File,
    length: u64,
    destination_offset: u64,
) -> std::io::Result<()> {
    let mut cursor = 0u64;
    let mut buffer = vec![0; CLONE_COPY_CHUNK_BYTES];
    while cursor < length {
        // SAFETY: source is owned and cursor fits off_t on x86_64.
        let data_offset = unsafe {
            libc::lseek(
                source.as_raw_fd(),
                cursor.try_into().unwrap(),
                libc::SEEK_DATA,
            )
        };
        let (data_offset, hole) = if data_offset == -1 {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ENXIO) => break,
                Some(libc::EINVAL) => (0, length),
                _ => return Err(error),
            }
        } else {
            // SAFETY: source is owned and data_offset came from lseek.
            let hole = unsafe { libc::lseek(source.as_raw_fd(), data_offset, libc::SEEK_HOLE) };
            if hole == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ENXIO) {
                    return Err(error);
                }
            }
            (
                data_offset as u64,
                if hole == -1 {
                    length
                } else {
                    (hole as u64).min(length)
                },
            )
        };

        let mut offset = data_offset;
        while offset < hole {
            let count = usize::try_from((hole - offset).min(buffer.len() as u64)).unwrap();
            source.read_exact_at(&mut buffer[..count], offset)?;
            destination.write_all_at(
                &buffer[..count],
                destination_offset.checked_add(offset).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "clone offset overflow")
                })?,
            )?;
            offset += count as u64;
        }
        cursor = hole;
    }
    Ok(())
}

fn clear_clone_destination_range(
    file: &std::fs::File,
    offset: u64,
    length: u64,
) -> std::io::Result<()> {
    if length == 0 {
        return Ok(());
    }
    // SAFETY: file is an owned regular-file descriptor and the range was
    // accepted by the recorded FICLONERANGE operation.
    let result = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset.try_into().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "clone offset overflow")
            })?,
            length.try_into().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "clone length overflow")
            })?,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if !matches!(
        error.raw_os_error(),
        Some(libc::EOPNOTSUPP | libc::ENOSYS | libc::EINVAL)
    ) {
        return Err(error);
    }

    tracing::warn!(%error, "hole punching unavailable; zeroing cloned replay range");
    let zeros = vec![0; CLONE_COPY_CHUNK_BYTES];
    let mut written = 0u64;
    while written < length {
        let count = usize::try_from((length - written).min(zeros.len() as u64)).unwrap();
        let write_offset = offset.checked_add(written).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "clone offset overflow")
        })?;
        file.write_all_at(&zeros[..count], write_offset)?;
        written += count as u64;
    }
    Ok(())
}

fn set_replay_output_flags(output_fd: RawFd, flags: libc::c_int) -> std::io::Result<()> {
    // SAFETY: the caller owns output_fd for the duration of the operation.
    if unsafe { libc::fcntl(output_fd, libc::F_SETFL, flags) } == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_replay_output_once_with<F>(
    output_fd: RawFd,
    bytes: &[u8],
    file_offset: Option<i64>,
    set_flags: &mut F,
) -> std::io::Result<usize>
where
    F: FnMut(RawFd, libc::c_int) -> std::io::Result<()>,
{
    // Nonblocking mode is temporary and is restored before this function
    // returns. In particular, no async suspension may expose it through the
    // shared open-file description.
    // SAFETY: fcntl only inspects this valid, Replayer-owned duplicate.
    let flags = unsafe { libc::fcntl(output_fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let temporary_flags = match file_offset {
        Some(_) => flags & !libc::O_APPEND,
        None => flags | libc::O_NONBLOCK,
    };
    let changed_flags = temporary_flags != flags;
    if changed_flags {
        set_flags(output_fd, temporary_flags)?;
    }

    let written = match file_offset {
        Some(position) => {
            // SAFETY: bytes points to readable memory and output_fd is open.
            unsafe { libc::pwrite(output_fd, bytes.as_ptr().cast(), bytes.len(), position) }
        }
        None => {
            // SAFETY: bytes points to readable memory and output_fd is open.
            unsafe { libc::write(output_fd, bytes.as_ptr().cast(), bytes.len()) }
        }
    };
    let result = if written == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(written as usize)
    };

    if changed_flags && let Err(restore_error) = set_flags(output_fd, flags) {
        let write_detail = match result {
            Ok(written) => format!("write completed with {written} byte(s)"),
            Err(ref write_error) => format!("write failed: {write_error}"),
        };
        return Err(std::io::Error::other(format!(
            "failed to restore replay output flags after {write_detail}: {restore_error}"
        )));
    }
    result
}

fn write_replay_output_once(
    output_fd: RawFd,
    bytes: &[u8],
    file_offset: Option<i64>,
) -> std::io::Result<usize> {
    let mut set_flags = set_replay_output_flags;
    write_replay_output_once_with(output_fd, bytes, file_offset, &mut set_flags)
}

async fn emit_replay_output_with<W>(
    output_fd: RawFd,
    bytes: &[u8],
    file_offset: Option<i64>,
    advances_output_offset: bool,
    write_once: &mut W,
) -> std::io::Result<()>
where
    W: FnMut(RawFd, &[u8], Option<i64>) -> std::io::Result<usize>,
{
    if bytes.is_empty() {
        return Ok(());
    }

    let mut offset = 0;
    while offset < bytes.len() {
        let remaining = &bytes[offset..];
        let written = if let Some(file_offset) = file_offset {
            let offset = i64::try_from(offset).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "recorded replay output offset does not fit in i64",
                )
            })?;
            let position = file_offset.checked_add(offset).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "recorded replay output offset overflow",
                )
            })?;
            write_once(output_fd, remaining, Some(position))
        } else {
            // send with MSG_NOSIGNAL handles sockets without risking a tracer
            // SIGPIPE. MSG_DONTWAIT is per-call and does not modify the shared
            // open-file description. Pipes reject send with ENOTSOCK, so use
            // a write whose O_NONBLOCK window ends before any async wait.
            let sent = unsafe {
                libc::send(
                    output_fd,
                    remaining.as_ptr().cast(),
                    remaining.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if sent == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ENOTSOCK) {
                    write_once(output_fd, remaining, None)
                } else {
                    Err(error)
                }
            } else {
                Ok(sent as usize)
            }
        };
        match written {
            Ok(written) if written > 0 => {
                offset += written;
                continue;
            }
            Err(error) => {
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    match wait_for_replay_output(output_fd).await {
                        Ok(true) => continue,
                        Ok(false) => {
                            return Err(std::io::Error::other(format!(
                                "replay output fd {output_fd} became unavailable after {offset}/{} bytes",
                                bytes.len()
                            )));
                        }
                        Err(wait_error) => {
                            return Err(std::io::Error::other(format!(
                                "replay output fd {output_fd} remained blocked after {offset}/{} bytes: {error}; readiness check failed: {wait_error}",
                                bytes.len()
                            )));
                        }
                    }
                }
                return Err(error);
            }
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    format!(
                        "replay output fd {output_fd} wrote zero bytes after {offset}/{} bytes",
                        bytes.len()
                    ),
                ));
            }
        }
    }

    if advances_output_offset {
        let file_offset = file_offset.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "advancing captured output requires a recorded file offset",
            )
        })?;
        let length = i64::try_from(bytes.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "recorded replay output length does not fit in i64",
            )
        })?;
        let final_offset = file_offset.checked_add(length).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "recorded replay output offset overflow",
            )
        })?;
        // SAFETY: output_fd is an owned seekable output duplicate.
        let positioned = unsafe { libc::lseek(output_fd, final_offset, libc::SEEK_SET) };
        if positioned == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if positioned != final_offset {
            return Err(std::io::Error::other(format!(
                "failed to advance captured output fd position to {final_offset}: positioned at {positioned}"
            )));
        }
    }
    Ok(())
}

async fn emit_replay_output(
    output_fd: RawFd,
    bytes: &[u8],
    file_offset: Option<i64>,
    advances_output_offset: bool,
) -> std::io::Result<()> {
    let mut write_once = write_replay_output_once;
    emit_replay_output_with(
        output_fd,
        bytes,
        file_offset,
        advances_output_offset,
        &mut write_once,
    )
    .await
}

fn truncate_replay_output(output_fd: RawFd, length: i64) -> Result<(), Error> {
    // SAFETY: output_fd is an owned duplicate and length was accepted by the
    // kernel while recording.
    if unsafe { libc::ftruncate(output_fd, length) } == -1 {
        Err(Error::Io(std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

fn successful_replay_write_count(result: Result<i64, Errno>) -> Result<Option<usize>, Error> {
    match result {
        Ok(count) => usize::try_from(count).map(Some).map_err(|_| {
            Error::Tool(anyhow::anyhow!(
                "recording contains successful write with invalid count {count}"
            ))
        }),
        Err(_) => Ok(None),
    }
}

/// Scatter the recorded flat output `bytes` of a vectored read back into the
/// guest's `iovec` array, filling each buffer in order until the bytes are
/// exhausted. Returns the number of bytes written (the syscall return value).
fn scatter_iovec_output<M: MemoryAccess>(
    memory: &mut M,
    iov_addr: Option<usize>,
    iovcnt: usize,
    bytes: &[u8],
) -> Result<usize, Errno> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let addr = iov_addr
        .and_then(Addr::<libc::iovec>::from_raw)
        .ok_or(Errno::EFAULT)?;
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        iovcnt
    ];
    memory.read_values(addr, &mut iovecs)?;

    let mut written = 0;
    for iovec in iovecs {
        if written == bytes.len() {
            break;
        }
        let take = (bytes.len() - written).min(iovec.iov_len);
        if take == 0 {
            continue;
        }
        let dst = AddrMut::<u8>::from_raw(iovec.iov_base as usize).ok_or(Errno::EFAULT)?;
        memory.write_exact(dst, &bytes[written..written + take])?;
        written += take;
    }
    // The recorded byte count must fit within the guest's provided iovecs.
    assert_eq!(written, bytes.len());
    Ok(written)
}

fn read_iovec_input<M: MemoryAccess>(
    memory: &M,
    iov_addr: Option<usize>,
    iovcnt: usize,
    length: usize,
) -> Result<Vec<u8>, Errno> {
    let mut bytes = vec![0; length];
    if length == 0 {
        return Ok(bytes);
    }
    let addr = iov_addr
        .and_then(Addr::<libc::iovec>::from_raw)
        .ok_or(Errno::EFAULT)?;
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        iovcnt
    ];
    memory.read_values(addr, &mut iovecs)?;

    let mut filled = 0;
    for iovec in iovecs {
        if filled == length {
            break;
        }
        let take = (length - filled).min(iovec.iov_len);
        if take == 0 {
            continue;
        }
        let src = Addr::<u8>::from_raw(iovec.iov_base as usize).ok_or(Errno::EFAULT)?;
        memory.read_exact(src, &mut bytes[filled..filled + take])?;
        filled += take;
    }
    assert_eq!(filled, length, "recorded write exceeds its iovec capacity");
    Ok(bytes)
}

fn read_write_bytes<M: MemoryAccess>(
    memory: &M,
    syscall: WriteFamily,
    length: usize,
) -> Result<Vec<u8>, Errno> {
    match syscall {
        WriteFamily::Write(call) => {
            let mut bytes = vec![0; length];
            memory.read_exact(call.buf().ok_or(Errno::EFAULT)?, &mut bytes)?;
            Ok(bytes)
        }
        WriteFamily::Pwrite64(call) => {
            let mut bytes = vec![0; length];
            memory.read_exact(call.buf().ok_or(Errno::EFAULT)?, &mut bytes)?;
            Ok(bytes)
        }
        WriteFamily::Writev(call) => read_iovec_input(
            memory,
            call.iov().map(|addr| addr.as_raw()),
            call.len(),
            length,
        ),
        WriteFamily::Pwritev(call) => read_iovec_input(
            memory,
            call.iov().map(|addr| addr.as_raw()),
            call.iov_len(),
            length,
        ),
        WriteFamily::Pwritev2(call) => read_iovec_input(
            memory,
            call.iov().map(|addr| addr.as_raw()),
            call.iov_len() as usize,
            length,
        ),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#662): Audit temporary blocking and restoration for replay side effects.
fn replay_side_effect_tool_error(
    fd: libc::c_int,
    operation: &str,
    outcome: impl std::fmt::Display,
) -> Error {
    Error::Tool(anyhow::anyhow!(
        "failed to {operation} for replay fd {fd}: {outcome}"
    ))
}

fn require_replay_fcntl_success(
    fd: libc::c_int,
    operation: &str,
    result: Result<i64, Errno>,
) -> Result<(), Error> {
    match result {
        Ok(0) => Ok(()),
        Ok(value) => Err(replay_side_effect_tool_error(
            fd,
            operation,
            format!("F_SETFL returned {value}"),
        )),
        Err(error) => Err(replay_side_effect_tool_error(fd, operation, error)),
    }
}

fn require_replay_fcntl_value(
    fd: libc::c_int,
    operation: &str,
    result: Result<i64, Errno>,
) -> Result<i64, Error> {
    result.map_err(|error| replay_side_effect_tool_error(fd, operation, error))
}

fn finish_kernel_side_effect(
    fd: libc::c_int,
    result: Result<i64, Errno>,
    restored: Result<i64, Errno>,
) -> Result<Result<i64, Errno>, Error> {
    require_replay_fcntl_success(fd, "restore descriptor flags", restored)?;
    // Keep the injected syscall result nested so callers can still compare it
    // exactly with the recording after the descriptor was restored.
    Ok(result)
}

async fn inject_kernel_side_effect<G: Guest<Replayer>>(
    guest: &mut G,
    fd: libc::c_int,
    syscall: Syscall,
) -> Result<Result<i64, Errno>, Error> {
    let result = guest.inject(syscall).await;
    if result != Err(Errno::EAGAIN) {
        return Ok(result);
    }

    let flags = require_replay_fcntl_value(
        fd,
        "read descriptor flags",
        guest
            .inject(Fcntl::new().with_fd(fd).with_cmd(FcntlCmd::F_GETFL))
            .await,
    )? as libc::c_int;
    if flags & libc::O_NONBLOCK == 0 {
        return Ok(result);
    }
    let cleared = guest
        .inject(
            Fcntl::new()
                .with_fd(fd)
                .with_cmd(FcntlCmd::F_SETFL(flags & !libc::O_NONBLOCK)),
        )
        .await;
    require_replay_fcntl_success(fd, "temporarily clear O_NONBLOCK", cleared)?;
    let result = guest.inject(syscall).await;
    let restored = guest
        .inject(Fcntl::new().with_fd(fd).with_cmd(FcntlCmd::F_SETFL(flags)))
        .await;
    finish_kernel_side_effect(fd, result, restored)
}

impl Replayer {
    fn advance_regular_file_position(&self, pid: reverie::Pid, fd: libc::c_int, length: usize) {
        if !self.fd_is_in_replay_root(pid, fd) {
            return;
        }
        let duplicate = crate::fd::duplicate_guest_fd(pid, fd)
            .unwrap_or_else(|error| panic!("failed to duplicate replay file for read: {error}"));
        let offset = libc::off_t::try_from(length).expect("recorded read length exceeds off_t");
        // SAFETY: duplicate is an owned descriptor and the recorded read succeeded.
        let result = unsafe { libc::lseek(duplicate.as_raw_fd(), offset, libc::SEEK_CUR) };
        assert_ne!(
            result,
            -1,
            "failed to advance replay file after read: {}",
            std::io::Error::last_os_error()
        );
    }

    fn replay_regular_file_write<G: Guest<Self>>(
        &self,
        guest: &G,
        syscall: WriteFamily,
        count: usize,
        offset: Option<i64>,
        advances_offset: bool,
    ) -> Result<(), Error> {
        if !self.fd_is_in_replay_root(guest.pid(), syscall.fd()) {
            return Ok(());
        }
        let offset = offset.ok_or_else(|| {
            Error::Tool(anyhow::anyhow!(
                "recorded regular-file write is missing its offset"
            ))
        })?;
        let offset_u64 = u64::try_from(offset).map_err(|_| {
            Error::Tool(anyhow::anyhow!(
                "recorded regular-file write used negative offset {offset}"
            ))
        })?;
        let bytes = read_write_bytes(&guest.memory(), syscall, count)?;
        let duplicate =
            crate::fd::duplicate_guest_fd(guest.pid(), syscall.fd()).map_err(Error::Io)?;
        let file = std::fs::File::from(duplicate);
        file.write_all_at(&bytes, offset_u64).map_err(Error::Io)?;

        if advances_offset {
            let next = offset
                .checked_add(i64::try_from(count).map_err(|_| {
                    Error::Tool(anyhow::anyhow!(
                        "recorded regular-file write length {count} exceeds i64"
                    ))
                })?)
                .ok_or_else(|| {
                    Error::Tool(anyhow::anyhow!(
                        "recorded regular-file write offset overflow"
                    ))
                })?;
            // SAFETY: file owns a duplicate of the guest open-file description.
            let result = unsafe { libc::lseek(file.as_raw_fd(), next, libc::SEEK_SET) };
            if result == -1 {
                return Err(Error::Io(std::io::Error::last_os_error()));
            }
            if result != next {
                return Err(Error::Tool(anyhow::anyhow!(
                    "failed to advance replay file after write to {next}: positioned at {result}"
                )));
            }
        }
        Ok(())
    }

    /// Replays the vectored read family (`readv`/`preadv`/`preadv2`) by
    /// scattering the recorded flattened output bytes across the guest's current
    /// `iovec` buffers. Guest-created regular files and eventfds are also read
    /// live so their kernel state remains aligned with the recording.
    pub(super) async fn handle_readv_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        iov_addr: Option<usize>,
        iovcnt: usize,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        let event = next_event!(guest, ReadvV2)?;
        let (fd, advances_offset) = match syscall {
            Syscall::Readv(call) => (call.fd(), true),
            Syscall::Preadv(call) => (call.fd(), false),
            Syscall::Preadv2(call) => {
                (call.fd(), vectored_offset(call.pos_l(), call.pos_h()) == -1)
            }
            _ => unreachable!("readv-family handler received {syscall:?}"),
        };
        match event.replay_fd_kind {
            ReplayFdKind::Eventfd => {
                let actual = inject_kernel_side_effect(guest, fd, syscall).await?;
                assert_eq!(
                    actual,
                    Ok(event.bytes.len() as i64),
                    "replayed readv eventfd side effect diverged"
                );
            }
            ReplayFdKind::RegularFile if advances_offset => {
                self.advance_regular_file_position(guest.pid(), fd, event.bytes.len());
            }
            ReplayFdKind::None | ReplayFdKind::RegularFile => {}
        }
        for _ in 0..event.consumed_sigpipe_count {
            self.consume_pending_sigpipe(guest).await?;
        }
        let written = scatter_iovec_output(&mut guest.memory(), iov_addr, iovcnt, &event.bytes)?;
        Ok(written as i64)
    }

    // FIXME: Generalize the read-family of syscalls with `ReadFamily`.
    pub(super) async fn handle_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Read,
    ) -> Result<i64, Error> {
        let event = next_event!(guest, ReadV2)?;
        match event.replay_fd_kind {
            ReplayFdKind::Eventfd => {
                let actual = inject_kernel_side_effect(guest, syscall.fd(), syscall.into()).await?;
                assert_eq!(
                    actual,
                    Ok(event.bytes.len() as i64),
                    "replayed read eventfd side effect diverged"
                );
            }
            ReplayFdKind::RegularFile => {
                self.advance_regular_file_position(guest.pid(), syscall.fd(), event.bytes.len());
            }
            ReplayFdKind::None => {}
        }
        for _ in 0..event.consumed_sigpipe_count {
            self.consume_pending_sigpipe(guest).await?;
        }

        assert!(event.bytes.len() <= syscall.len());

        guest
            .memory()
            .write_exact(syscall.buf().unwrap(), &event.bytes)
            .unwrap();
        Ok(event.bytes.len() as i64)
    }

    pub(super) async fn handle_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Pread64,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        assert!(buf.len() <= syscall.len());

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.buf().unwrap(), &buf)
            .unwrap();
        Ok(buf.len() as i64)
    }

    async fn consume_pending_sigpipe<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: set is initialized and remains valid for both libc calls.
        assert_eq!(unsafe { libc::sigemptyset(&mut set) }, 0);
        assert_eq!(unsafe { libc::sigaddset(&mut set, libc::SIGPIPE) }, 0);

        let mut stack = guest.stack().await;
        let set_addr = stack.reserve::<libc::sigset_t>();
        let info_addr = stack.reserve::<libc::siginfo_t>();
        let timeout_addr = stack.push(Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        });
        let _guard = stack.commit()?;
        guest.memory().write_value(set_addr, &set)?;
        let consumed = guest
            .inject(
                RtSigtimedwait::new()
                    .with_set(Some(set_addr))
                    .with_info(Some(info_addr))
                    .with_timeout(Some(timeout_addr))
                    .with_sigsetsize(std::mem::size_of::<u64>()),
            )
            .await;
        assert_eq!(
            consumed,
            Ok(libc::SIGPIPE as i64),
            "failed to consume replayed SIGPIPE after signalfd read: {consumed:?}"
        );
        Ok(())
    }
    async fn replay_sigpipe<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        let uid = guest.inject(Getuid::new()).await? as libc::uid_t;

        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert!(std::mem::size_of::<UserSignalInfoHead>() <= std::mem::size_of_val(&info));
        let head =
            unsafe { &mut *(&mut info as *mut libc::siginfo_t).cast::<UserSignalInfoHead>() };
        *head = UserSignalInfoHead {
            signo: libc::SIGPIPE,
            errno: 0,
            code: libc::SI_USER,
            padding: 0,
            pid: guest.pid().as_raw(),
            uid,
        };

        let mut stack = guest.stack().await;
        let info_addr = stack.reserve::<libc::siginfo_t>();
        let _guard = stack.commit()?;
        guest.memory().write_value(info_addr, &info)?;
        let delivered = guest
            .inject(
                RtTgsigqueueinfo::new()
                    .with_tgid(guest.pid().as_raw())
                    .with_tid(guest.tid().as_raw())
                    .with_sig(libc::SIGPIPE)
                    .with_siginfo(Some(info_addr)),
            )
            .await;
        assert_eq!(
            delivered,
            Ok(0),
            "failed to reproduce recorded SIGPIPE: {delivered:?}"
        );
        Ok(())
    }

    fn output_endpoint(
        &self,
        output_fd: libc::c_int,
    ) -> Result<(&tokio::sync::Mutex<()>, RawFd), Error> {
        let (lock, output, error) = match output_fd {
            libc::STDOUT_FILENO => (&self.stdout_output_lock, &self.stdout, &self.stdout_error),
            libc::STDERR_FILENO => (&self.stderr_output_lock, &self.stderr, &self.stderr_error),
            _ => {
                return Err(Error::Tool(anyhow::anyhow!(
                    "recording names invalid captured output descriptor {output_fd}"
                )));
            }
        };
        let output = output.as_ref().ok_or_else(|| {
            let reason = error.as_deref().unwrap_or("descriptor was closed");
            Error::Tool(anyhow::anyhow!(
                "recording requires output fd {output_fd}, but replay could not duplicate it: {reason}"
            ))
        })?;
        Ok((lock, output.as_raw_fd()))
    }

    async fn replay_output<G: Guest<Self>>(
        &self,
        guest: &mut G,
        advances_output_offset: bool,
        syscall: WriteFamily,
        output_fd: i32,
        count: usize,
        output_offset: Option<i64>,
    ) -> Result<(), Error> {
        // Faults while reading the guest's syscall arguments are Linux-visible
        // syscall errors. Failures after that boundary are failures of Replay's
        // host-side output machinery and must abort the tool instead.
        let bytes = read_write_bytes(&guest.memory(), syscall, count)?;
        let (output_lock, output) = self.output_endpoint(output_fd)?;
        let _guard = output_lock.lock().await;
        emit_replay_output(output, &bytes, output_offset, advances_output_offset)
            .await
            .map_err(Error::Io)?;
        Ok(())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#557): Audit recorded write side effects and signal fidelity.
    pub(super) async fn handle_write_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: WriteFamily,
    ) -> Result<i64, Error> {
        let event = next_event!(guest, WriteV2)?;
        let successful_count = successful_replay_write_count(event.result)?;
        match event.replay_fd_kind {
            ReplayFdKind::Eventfd => {
                let actual =
                    inject_kernel_side_effect(guest, syscall.fd(), Syscall::from(syscall)).await?;
                assert_eq!(
                    actual, event.result,
                    "replayed eventfd write side effect diverged"
                );
            }
            ReplayFdKind::RegularFile => {
                if let Some(count) = successful_count {
                    self.replay_regular_file_write(
                        guest,
                        syscall,
                        count,
                        event.replay_file_offset,
                        event.replay_file_advances_offset,
                    )?;
                }
            }
            ReplayFdKind::None => {}
        }
        if event.generated_sigpipe {
            self.replay_sigpipe(guest).await?;
        }
        if let (Some(count), Some(output_fd)) = (successful_count, event.output_fd) {
            self.replay_output(
                guest,
                event.advances_output_offset,
                syscall,
                output_fd,
                count,
                event.output_offset,
            )
            .await?;
        }
        event.result.map_err(Error::from)
    }

    // TODO-HUMAN-REVIEW(#557): Audit captured-output ftruncate replay.
    pub(super) async fn handle_ftruncate<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ftruncate,
    ) -> Result<i64, Error> {
        let event = next_event!(guest, FtruncateV2)?;
        if event.result.is_ok() {
            if event.length != syscall.length() {
                return Err(Error::Tool(anyhow::anyhow!(
                    "replayed ftruncate length diverged: recorded {}, requested {}",
                    event.length,
                    syscall.length()
                )));
            }
            if let Some(output_fd) = event.output_fd {
                let (output_lock, output) = self.output_endpoint(output_fd)?;
                let _guard = output_lock.lock().await;
                truncate_replay_output(output, event.length)?;
            }
            if event.replay_regular_file
                && event.output_fd.is_none()
                && self.fd_is_in_replay_root(guest.pid(), syscall.fd())
            {
                let duplicate =
                    crate::fd::duplicate_guest_fd(guest.pid(), syscall.fd()).map_err(Error::Io)?;
                let file = std::fs::File::from(duplicate);
                let length = u64::try_from(event.length).map_err(|_| {
                    Error::Tool(anyhow::anyhow!(
                        "recording contains successful ftruncate with negative length {}",
                        event.length
                    ))
                })?;
                file.set_len(length).map_err(Error::Io)?;
            }
        }
        event.result.map_err(Error::from)
    }

    pub(super) async fn handle_stat_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: StatFamily,
    ) -> Result<i64, Errno> {
        next_event!(guest, Stat).and_then(|event| {
            let addr = syscall.stat().ok_or(Errno::EFAULT)?;
            guest.memory().write_value(addr.0, &event.statbuf)?;
            // stat calls always return 0 on success.
            Ok(0)
        })
    }

    pub(super) async fn handle_statfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        buf: Option<AddrMut<'_, libc::statfs>>,
    ) -> Result<i64, Errno> {
        let bytes = next_event!(guest, Statfs)?;
        assert_eq!(bytes.len(), std::mem::size_of::<libc::statfs>());
        guest
            .memory()
            .write_exact(buf.ok_or(Errno::EFAULT)?.cast(), &bytes)?;
        Ok(0)
    }

    pub(super) async fn handle_statx<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Statx,
    ) -> Result<i64, Errno> {
        next_event!(guest, Statx).and_then(|buf| {
            let addr = syscall.statx().ok_or(Errno::EFAULT)?;
            guest.memory().write_value(addr.0, &buf.into())?;
            // statx calls always return 0 on success.
            Ok(0)
        })
    }

    pub(super) async fn handle_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ioctl,
    ) -> Result<i64, Errno> {
        let request = syscall.request();

        if deterministic_ioctl_error(&request).is_some() {
            return next_event!(guest, Return);
        }

        if matches!(
            request,
            ioctl::Request::FICLONE(_) | ioctl::Request::FICLONERANGE(_)
        ) {
            let snapshot = next_event!(guest, FileClone)?;
            let destination_is_internal = self.fd_is_in_replay_root(guest.pid(), syscall.fd());
            if destination_is_internal {
                let path = format!("/proc/{}/fd/{}", guest.pid().as_raw(), syscall.fd());
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap_or_else(|error| {
                        panic!("failed to open cloned replay destination {path}: {error}")
                    });
                let prior_length = file
                    .metadata()
                    .unwrap_or_else(|error| panic!("failed to stat clone destination: {error}"))
                    .len();
                if snapshot.truncate_destination {
                    file.set_len(0).unwrap_or_else(|error| {
                        panic!("failed to truncate cloned replay file: {error}")
                    });
                }
                file.set_len(snapshot.length)
                    .unwrap_or_else(|error| panic!("failed to size cloned replay file: {error}"));
                if !snapshot.truncate_destination && snapshot.destination_offset < prior_length {
                    let overlap = snapshot
                        .replacement_length
                        .min(prior_length - snapshot.destination_offset);
                    clear_clone_destination_range(&file, snapshot.destination_offset, overlap)
                        .unwrap_or_else(|error| {
                            panic!("failed to clear cloned replay range: {error}")
                        });
                }
                match snapshot.image {
                    FileCloneImage::Extents(extents) => {
                        for extent in extents {
                            let offset = snapshot
                                .destination_offset
                                .checked_add(extent.offset)
                                .expect("clone destination offset overflow");
                            file.write_all_at(&extent.bytes, offset)
                                .unwrap_or_else(|error| {
                                    panic!("failed to materialize cloned replay extent: {error}")
                                });
                        }
                    }
                    FileCloneImage::Sidecar(relative) => {
                        let relative = std::path::Path::new(&relative);
                        assert!(
                            !relative.is_absolute()
                                && !relative.components().any(|component| matches!(
                                    component,
                                    std::path::Component::ParentDir | std::path::Component::RootDir
                                )),
                            "invalid clone sidecar path {relative:?}"
                        );
                        let sidecar = self.data.join(relative);
                        let source = std::fs::File::open(&sidecar).unwrap_or_else(|error| {
                            panic!("failed to open clone sidecar {sidecar:?}: {error}")
                        });
                        let sidecar_length = source
                            .metadata()
                            .unwrap_or_else(|error| {
                                panic!("failed to stat clone sidecar {sidecar:?}: {error}")
                            })
                            .len();
                        assert_eq!(
                            sidecar_length, snapshot.replacement_length,
                            "clone sidecar length changed"
                        );
                        restore_sparse_clone_sidecar(
                            &source,
                            &file,
                            snapshot.replacement_length,
                            snapshot.destination_offset,
                        )
                        .unwrap_or_else(|error| {
                            panic!("failed to restore clone sidecar {sidecar:?}: {error}")
                        });
                    }
                }
            }
            Ok(0)
        } else if matches!(
            request,
            ioctl::Request::FIOCLEX | ioctl::Request::FIONCLEX | ioctl::Request::FIONBIO(_)
        ) {
            self.handle_replayed_side_effect(guest, Syscall::from(syscall), "ioctl")
                .await
        } else if request.direction() == ioctl::Direction::Read {
            let output = next_event!(guest, Ioctl)?;
            request.write_output(&mut guest.memory(), &output)?;
            Ok(0)
        } else {
            let ret = next_event!(guest, Return)?;
            Ok(ret)
        }
    }

    pub(super) async fn handle_readlink<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Readlink,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        debug_assert!(buf.len() <= syscall.bufsize());

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.buf().unwrap().cast::<u8>(), &buf)?;
        Ok(buf.len() as i64)
    }

    pub(super) async fn handle_getdents<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Getdents,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        // Make sure we don't overflow the buffer.
        debug_assert!(buf.len() <= syscall.count() as usize);

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.dirent().unwrap().cast::<u8>(), &buf)?;
        Ok(buf.len() as i64)
    }

    pub(super) async fn handle_getdents64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Getdents64,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        // Make sure we don't overflow the buffer.
        debug_assert!(buf.len() <= syscall.count() as usize);

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.dirent().unwrap().cast::<u8>(), &buf)?;
        Ok(buf.len() as i64)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs::OpenOptions;
    use std::io::Read as _;
    use std::io::Seek as _;
    use std::io::Write as _;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use tokio::io::unix::AsyncFd;

    use super::*;

    #[test]
    fn successful_kernel_side_effect_restore_preserves_the_injected_result() {
        assert_eq!(finish_kernel_side_effect(4, Ok(7), Ok(0)).unwrap(), Ok(7));
        assert_eq!(
            finish_kernel_side_effect(4, Err(Errno::EAGAIN), Ok(0)).unwrap(),
            Err(Errno::EAGAIN)
        );
    }

    #[test]
    fn kernel_side_effect_restore_failure_is_not_a_guest_errno() {
        for restored in [Err(Errno::EBADF), Ok(1)] {
            let failure = finish_kernel_side_effect(4, Ok(7), restored)
                .expect_err("restoration failure must abort replay");
            assert!(
                matches!(failure.into_errno(), Err(Error::Tool(_))),
                "restoration failure became guest-visible errno"
            );
        }
    }

    #[test]
    fn temporary_flag_change_failure_is_not_a_guest_errno() {
        for result in [Err(Errno::EBADF), Ok(1)] {
            let failure = require_replay_fcntl_success(4, "temporarily clear O_NONBLOCK", result)
                .expect_err("flag setup failure must abort replay");
            assert!(
                matches!(failure.into_errno(), Err(Error::Tool(_))),
                "flag setup failure became guest-visible errno"
            );
        }
    }

    #[test]
    fn descriptor_flag_read_failure_is_not_a_guest_errno() {
        let failure = require_replay_fcntl_value(4, "read descriptor flags", Err(Errno::EBADF))
            .expect_err("flag read failure must abort replay");
        assert!(
            matches!(failure.into_errno(), Err(Error::Tool(_))),
            "flag read failure became guest-visible errno"
        );
    }

    #[test]
    fn missing_or_invalid_output_endpoint_is_an_outer_tool_error() {
        let replayer = Replayer {
            stdout_error: Some("Too many open files".to_owned()),
            ..Replayer::default()
        };
        for output_fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO, 9] {
            let failure = match replayer.output_endpoint(output_fd) {
                Ok(_) => panic!("unavailable replay output fd {output_fd} unexpectedly resolved"),
                Err(error) => error,
            };
            if output_fd == libc::STDOUT_FILENO {
                assert!(
                    failure.to_string().contains("Too many open files"),
                    "endpoint acquisition failure lost its recorded cause: {failure}"
                );
            }
            assert!(
                matches!(failure.into_errno(), Err(Error::Tool(_))),
                "endpoint acquisition failure became guest-visible errno"
            );
        }
    }

    #[tokio::test]
    async fn replay_output_sink_failure_is_an_outer_io_error() {
        let output = OpenOptions::new().write(true).open("/dev/full").unwrap();

        let failure = emit_replay_output(output.as_raw_fd(), b"LOST", None, false)
            .await
            .map_err(Error::Io)
            .expect_err("full replay output device must abort replay");
        assert!(
            matches!(failure.into_errno(), Err(Error::Io(error)) if error.raw_os_error() == Some(libc::ENOSPC)),
            "replay output sink failure became guest-visible errno"
        );
    }

    #[test]
    fn replay_output_ftruncate_failure_is_an_outer_io_error() {
        let failure = truncate_replay_output(-1, 0)
            .expect_err("invalid captured output endpoint must abort replay");
        assert!(
            matches!(failure.into_errno(), Err(Error::Io(error)) if error.raw_os_error() == Some(libc::EBADF)),
            "captured-output ftruncate failure became guest-visible errno"
        );
    }

    #[tokio::test]
    async fn replay_output_readiness_failure_is_an_outer_io_error() {
        let failure = wait_for_replay_output(-1)
            .await
            .map_err(Error::Io)
            .expect_err("invalid replay output must fail readiness acquisition");
        assert!(
            matches!(failure.into_errno(), Err(Error::Io(error)) if error.raw_os_error() == Some(libc::EBADF)),
            "readiness failure became guest-visible errno"
        );
    }

    #[test]
    fn malformed_successful_write_count_is_an_outer_tool_error() {
        let failure = successful_replay_write_count(Ok(-1))
            .expect_err("negative successful count must invalidate the recording");
        assert!(
            matches!(failure.into_errno(), Err(Error::Tool(_))),
            "invalid recorded count became guest-visible errno"
        );
        assert_eq!(
            successful_replay_write_count(Err(Errno::EFAULT)).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn replay_output_preserves_regular_file_offset() {
        let mut file = tempfile::tempfile().unwrap();
        emit_replay_output(file.as_raw_fd(), b"ONE", None, false)
            .await
            .unwrap();
        emit_replay_output(file.as_raw_fd(), b"TWO", None, false)
            .await
            .unwrap();
        file.rewind().unwrap();

        let mut output = String::new();
        file.read_to_string(&mut output).unwrap();
        assert_eq!(output, "ONETWO");
    }

    #[tokio::test]
    async fn replay_output_preserves_positioned_file_writes() {
        let mut file = tempfile::tempfile().unwrap();
        emit_replay_output(file.as_raw_fd(), b"X", Some(5), false)
            .await
            .unwrap();
        file.rewind().unwrap();

        let mut output = Vec::new();
        file.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"\0\0\0\0\0X");
    }

    #[tokio::test]
    async fn positioned_replay_advances_shared_offset_for_write() {
        let mut file = tempfile::tempfile().unwrap();
        emit_replay_output(file.as_raw_fd(), b"X", Some(5), true)
            .await
            .unwrap();
        assert_eq!(file.stream_position().unwrap(), 6);
        file.rewind().unwrap();

        let mut output = Vec::new();
        file.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"\0\0\0\0\0X");
    }

    #[tokio::test]
    async fn positioned_replay_temporarily_clears_append() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"ABC").unwrap();
        let fd = file.as_raw_fd();
        // SAFETY: fd is open and F_GETFL does not mutate memory.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(flags, -1);
        // SAFETY: fd is open for the duration of the test.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_APPEND) },
            -1
        );

        emit_replay_output(fd, b"X", Some(1), false).await.unwrap();
        file.rewind().unwrap();
        let mut output = String::new();
        file.read_to_string(&mut output).unwrap();
        assert_eq!(output, "AXC");
        // SAFETY: fd remains open and F_GETFL does not mutate memory.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_APPEND,
            0
        );
    }

    #[tokio::test]
    async fn replay_output_supports_sockets() {
        let (output, mut peer) = UnixStream::pair().unwrap();
        emit_replay_output(output.as_raw_fd(), b"SOCKET_OUT", None, false)
            .await
            .unwrap();

        let mut received = [0; 10];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"SOCKET_OUT");
    }

    #[tokio::test]
    async fn replay_output_reports_closed_socket_failure() {
        let (output, peer) = UnixStream::pair().unwrap();
        // A concurrent fork can keep the peer alive after drop(peer). Hold a
        // duplicate to exercise that case, and shut down their shared socket.
        let inherited_peer = peer.try_clone().unwrap();
        peer.shutdown(std::net::Shutdown::Both).unwrap();
        drop(peer);

        let error = emit_replay_output(output.as_raw_fd(), b"LOST", None, false)
            .await
            .expect_err("closed replay output socket must fail");
        assert_eq!(error.raw_os_error(), Some(libc::EPIPE));
        drop(inherited_peer);
    }

    #[tokio::test]
    async fn replay_output_reports_full_device_failure() {
        let output = OpenOptions::new().write(true).open("/dev/full").unwrap();

        let error = emit_replay_output(output.as_raw_fd(), b"LOST", None, false)
            .await
            .expect_err("full replay output device must fail");
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replay_output_does_not_retry_after_flag_restoration_failure() {
        let mut pipe = [0; 2];
        // SAFETY: pipe points to two writable integers.
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // SAFETY: ownership of each open pipe descriptor transfers exactly once.
        let _input = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
        let output = unsafe { OwnedFd::from_raw_fd(pipe[1]) };

        // Fill the pipe while it is nonblocking, then restore blocking mode so
        // replay has to change and restore the shared open-file-description flags.
        let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        set_replay_output_flags(output.as_raw_fd(), flags | libc::O_NONBLOCK).unwrap();
        let fill = [0_u8; 4096];
        loop {
            let written =
                unsafe { libc::write(output.as_raw_fd(), fill.as_ptr().cast(), fill.len()) };
            if written >= 0 {
                continue;
            }
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock
            );
            break;
        }
        set_replay_output_flags(output.as_raw_fd(), flags).unwrap();

        let set_flags_calls = Cell::new(0);
        let mut set_flags = |fd, requested_flags| {
            let call = set_flags_calls.get() + 1;
            set_flags_calls.set(call);
            if call == 2 {
                Err(std::io::Error::from_raw_os_error(libc::EBADF))
            } else {
                set_replay_output_flags(fd, requested_flags)
            }
        };
        let mut write_once = |fd, bytes: &[u8], offset| {
            write_replay_output_once_with(fd, bytes, offset, &mut set_flags)
        };

        let error = tokio::time::timeout(
            Duration::from_millis(250),
            emit_replay_output_with(output.as_raw_fd(), b"x", None, false, &mut write_once),
        )
        .await
        .expect("flag restoration failure was incorrectly retried")
        .expect_err("flag restoration failure must fail replay output");
        assert_eq!(set_flags_calls.get(), 2);
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        let rendered = error.to_string();
        assert!(
            rendered.contains("failed to restore replay output flags"),
            "missing restoration context: {rendered}"
        );
        assert!(
            rendered.contains(&format!("os error {}", libc::EBADF)),
            "missing EBADF cause: {rendered}"
        );
        let failure = Error::Io(error);
        assert!(
            matches!(failure.into_errno(), Err(Error::Io(_))),
            "flag-restoration failure became guest-visible errno"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replay_output_retries_after_blocking_pipe_backpressure_on_same_executor() {
        let mut pipe = [0; 2];
        // SAFETY: pipe points to two writable integers.
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // SAFETY: ownership of each open pipe descriptor transfers exactly once.
        let input = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(pipe[0]) }).unwrap();
        let output = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
        // AsyncFd readers must use a nonblocking descriptor. The write end
        // deliberately remains blocking to exercise temporary flag handling.
        let input_flags = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(input_flags, -1);
        assert_ne!(
            unsafe {
                libc::fcntl(
                    input.as_raw_fd(),
                    libc::F_SETFL,
                    input_flags | libc::O_NONBLOCK,
                )
            },
            -1
        );
        let output_flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(output_flags & libc::O_NONBLOCK, 0);

        let expected = vec![b'x'; 256 * 1024];
        let expected_for_reader = expected.clone();
        let reader = tokio::spawn(async move {
            let mut actual = vec![0; expected_for_reader.len()];
            let mut offset = 0;
            while offset < actual.len() {
                let mut readiness = input.readable().await.unwrap();
                match readiness.try_io(|input| {
                    // SAFETY: actual's unwritten suffix is valid and the descriptor is open.
                    let read = unsafe {
                        libc::read(
                            input.get_ref().as_raw_fd(),
                            actual[offset..].as_mut_ptr().cast(),
                            actual.len() - offset,
                        )
                    };
                    if read == -1 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(read as usize)
                    }
                }) {
                    Ok(Ok(0)) => break,
                    Ok(Ok(read)) => offset += read,
                    Ok(Err(error)) => panic!("pipe read failed: {error}"),
                    Err(_) => continue,
                }
            }
            actual.truncate(offset);
            actual
        });

        let actual = tokio::time::timeout(Duration::from_secs(2), async {
            emit_replay_output(output.as_raw_fd(), &expected, None, false)
                .await
                .unwrap();
            assert_eq!(
                unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
                0,
                "replay output left the caller's pipe nonblocking"
            );
            drop(output);
            reader.await.unwrap()
        })
        .await
        .expect("replay output deadlocked its pipe reader");
        assert_eq!(actual, expected);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replay_output_cancellation_restores_blocking_pipe_flags() {
        let mut pipe = [0; 2];
        // SAFETY: pipe points to two writable integers.
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // SAFETY: ownership of each open pipe descriptor transfers exactly once.
        let _input = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
        let output = unsafe { OwnedFd::from_raw_fd(pipe[1]) };

        // Fill the pipe without blocking, then restore its blocking mode before
        // calling the async replay path.
        let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(
            unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            -1
        );
        let fill = [0_u8; 4096];
        loop {
            let written =
                unsafe { libc::write(output.as_raw_fd(), fill.as_ptr().cast(), fill.len()) };
            if written >= 0 {
                continue;
            }
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock
            );
            break;
        }
        assert_ne!(
            unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags) },
            -1
        );

        let result = tokio::time::timeout(
            Duration::from_millis(25),
            emit_replay_output(output.as_raw_fd(), b"x", None, false),
        )
        .await;
        assert!(
            result.is_err(),
            "full pipe unexpectedly accepted replay output"
        );
        assert_eq!(
            unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0,
            "cancelled replay output left the caller's pipe nonblocking"
        );
    }
}
