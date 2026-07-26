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
async fn wait_for_replay_output(output_fd: RawFd) -> bool {
    // F_DUPFD_CLOEXEC keeps the endpoint alive while the bounded blocking task
    // polls it, without changing the shared open-file-description flags.
    let duplicate = unsafe { libc::fcntl(output_fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate == -1 {
        tracing::debug!(
            error = %std::io::Error::last_os_error(),
            output_fd,
            "could not duplicate replay output for readiness polling"
        );
        return false;
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
        Ok(Ok(ready)) => ready,
        Ok(Err(error)) => {
            tracing::debug!(%error, output_fd, "could not wait for replay output capacity");
            false
        }
        Err(error) => {
            tracing::debug!(%error, output_fd, "could not monitor replay output capacity");
            false
        }
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

fn write_replay_output_once(
    output_fd: RawFd,
    bytes: &[u8],
    file_offset: Option<i64>,
) -> std::io::Result<usize> {
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
        // SAFETY: the descriptor remains open and this function does not
        // suspend before restoring its flags.
        if unsafe { libc::fcntl(output_fd, libc::F_SETFL, temporary_flags) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
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

    if changed_flags {
        // SAFETY: restore the shared description before any async wait.
        if unsafe { libc::fcntl(output_fd, libc::F_SETFL, flags) } == -1 {
            tracing::debug!(
                error = %std::io::Error::last_os_error(),
                output_fd,
                "could not restore replay output flags"
            );
        }
    }
    result
}

async fn emit_replay_output(
    output_fd: RawFd,
    bytes: &[u8],
    file_offset: Option<i64>,
    advances_output_offset: bool,
) {
    if bytes.is_empty() {
        return;
    }

    let mut offset = 0;
    while offset < bytes.len() {
        let remaining = &bytes[offset..];
        let written = if let Some(file_offset) = file_offset {
            let position = file_offset
                .checked_add(offset as i64)
                .expect("recorded output offset overflow");
            write_replay_output_once(output_fd, remaining, Some(position))
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
                    write_replay_output_once(output_fd, remaining, None)
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
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && wait_for_replay_output(output_fd).await
                {
                    continue;
                }
                tracing::debug!(%error, output_fd, "could not emit all replay output");
            }
            Ok(_) => {}
        }
        break;
    }

    if advances_output_offset {
        let final_offset = file_offset
            .expect("advancing captured output requires a recorded file offset")
            .checked_add(bytes.len() as i64)
            .expect("recorded output offset overflow");
        // SAFETY: output_fd is an owned seekable output duplicate.
        let positioned = unsafe { libc::lseek(output_fd, final_offset, libc::SEEK_SET) };
        assert_eq!(
            positioned,
            final_offset,
            "failed to advance captured output fd position: {}",
            std::io::Error::last_os_error()
        );
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

fn vectored_offset(low: u64, high: u64) -> i64 {
    if std::mem::size_of::<usize>() == 8 {
        low as i64
    } else {
        ((high << 32) | (low & u32::MAX as u64)) as i64
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#662): Audit temporary blocking and restoration for replay side effects.
async fn inject_kernel_side_effect<G: Guest<Replayer>>(
    guest: &mut G,
    fd: libc::c_int,
    syscall: Syscall,
) -> Result<i64, Errno> {
    let result = guest.inject(syscall).await;
    if result != Err(Errno::EAGAIN) {
        return result;
    }

    let flags = guest
        .inject(Fcntl::new().with_fd(fd).with_cmd(FcntlCmd::F_GETFL))
        .await? as libc::c_int;
    if flags & libc::O_NONBLOCK == 0 {
        return result;
    }
    guest
        .inject(
            Fcntl::new()
                .with_fd(fd)
                .with_cmd(FcntlCmd::F_SETFL(flags & !libc::O_NONBLOCK)),
        )
        .await?;
    let result = guest.inject(syscall).await;
    let restored = guest
        .inject(Fcntl::new().with_fd(fd).with_cmd(FcntlCmd::F_SETFL(flags)))
        .await;
    assert_eq!(restored, Ok(0), "failed to restore replay fd flags");
    result
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
    ) -> Result<(), Errno> {
        if !self.fd_is_in_replay_root(guest.pid(), syscall.fd()) {
            return Ok(());
        }
        let offset = offset.expect("recorded regular-file write is missing its offset");
        let offset_u64 = u64::try_from(offset).expect("recorded write used a negative offset");
        let bytes = read_write_bytes(&guest.memory(), syscall, count)?;
        let duplicate = crate::fd::duplicate_guest_fd(guest.pid(), syscall.fd())
            .unwrap_or_else(|error| panic!("failed to duplicate replay file for write: {error}"));
        let file = std::fs::File::from(duplicate);
        file.write_all_at(&bytes, offset_u64)
            .unwrap_or_else(|error| panic!("failed to materialize replay write: {error}"));

        if advances_offset {
            let next = offset
                .checked_add(i64::try_from(count).expect("recorded write length exceeds i64"))
                .expect("recorded write offset overflow");
            // SAFETY: file owns a duplicate of the guest open-file description.
            let result = unsafe { libc::lseek(file.as_raw_fd(), next, libc::SEEK_SET) };
            assert_eq!(
                result,
                next,
                "failed to advance replay file after write: {}",
                std::io::Error::last_os_error()
            );
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
    ) -> Result<i64, Errno> {
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
                let actual = inject_kernel_side_effect(guest, fd, syscall).await;
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
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, ReadV2)?;
        match event.replay_fd_kind {
            ReplayFdKind::Eventfd => {
                let actual = inject_kernel_side_effect(guest, syscall.fd(), syscall.into()).await;
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

    fn output_endpoint(&self, output_fd: libc::c_int) -> libc::c_int {
        let (output, error) = match output_fd {
            libc::STDOUT_FILENO => (&self.stdout, &self.stdout_error),
            libc::STDERR_FILENO => (&self.stderr, &self.stderr_error),
            _ => panic!("invalid recorded output descriptor {output_fd}"),
        };
        output.as_ref().map(AsRawFd::as_raw_fd).unwrap_or_else(|| {
            let reason = error.as_deref().unwrap_or("descriptor was closed");
            panic!(
                "recording requires output fd {output_fd}, but replay could not duplicate it: {reason}"
            )
        })
    }

    async fn replay_output<G: Guest<Self>>(
        &self,
        guest: &mut G,
        advances_output_offset: bool,
        syscall: WriteFamily,
        output_fd: i32,
        count: usize,
        output_offset: Option<i64>,
    ) -> Result<(), Errno> {
        let bytes = read_write_bytes(&guest.memory(), syscall, count)?;
        let output_lock = match output_fd {
            libc::STDOUT_FILENO => &self.stdout_output_lock,
            libc::STDERR_FILENO => &self.stderr_output_lock,
            _ => panic!("invalid recorded output descriptor {output_fd}"),
        };
        let _guard = output_lock.lock().await;
        let output = self.output_endpoint(output_fd);
        emit_replay_output(output, &bytes, output_offset, advances_output_offset).await;
        Ok(())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#557): Audit recorded write side effects and signal fidelity.
    pub(super) async fn handle_write_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: WriteFamily,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, WriteV2)?;
        match event.replay_fd_kind {
            ReplayFdKind::Eventfd => {
                let actual =
                    inject_kernel_side_effect(guest, syscall.fd(), Syscall::from(syscall)).await;
                assert_eq!(
                    actual, event.result,
                    "replayed eventfd write side effect diverged"
                );
            }
            ReplayFdKind::RegularFile => {
                if let Ok(count) = event.result {
                    self.replay_regular_file_write(
                        guest,
                        syscall,
                        usize::try_from(count).expect("negative successful write count"),
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
        if let (Ok(count), Some(output_fd)) = (event.result, event.output_fd) {
            self.replay_output(
                guest,
                event.advances_output_offset,
                syscall,
                output_fd,
                count as usize,
                event.output_offset,
            )
            .await?;
        }
        event.result
    }

    // TODO-HUMAN-REVIEW(#557): Audit captured-output ftruncate replay.
    pub(super) fn handle_ftruncate<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ftruncate,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, FtruncateV2)?;
        if event.result.is_ok() {
            assert_eq!(event.length, syscall.length());
            if let Some(output_fd) = event.output_fd {
                let output = self.output_endpoint(output_fd);
                // SAFETY: output is an owned duplicate and event.length was
                // accepted by the kernel during recording.
                let truncated = unsafe { libc::ftruncate(output, event.length) };
                assert_eq!(
                    truncated,
                    0,
                    "failed to reproduce ftruncate on captured output fd {output_fd}: {}",
                    std::io::Error::last_os_error()
                );
            }
            if event.replay_regular_file
                && event.output_fd.is_none()
                && self.fd_is_in_replay_root(guest.pid(), syscall.fd())
            {
                let duplicate = crate::fd::duplicate_guest_fd(guest.pid(), syscall.fd())
                    .unwrap_or_else(|error| {
                        panic!("failed to duplicate replay file for ftruncate: {error}")
                    });
                let file = std::fs::File::from(duplicate);
                file.set_len(
                    u64::try_from(event.length).expect("successful ftruncate used negative length"),
                )
                .unwrap_or_else(|error| panic!("failed to materialize replay ftruncate: {error}"));
            }
        }
        event.result
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
    use std::io::Read as _;
    use std::io::Seek as _;
    use std::io::Write as _;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use tokio::io::unix::AsyncFd;

    use super::*;

    #[tokio::test]
    async fn replay_output_preserves_regular_file_offset() {
        let mut file = tempfile::tempfile().unwrap();
        emit_replay_output(file.as_raw_fd(), b"ONE", None, false).await;
        emit_replay_output(file.as_raw_fd(), b"TWO", None, false).await;
        file.rewind().unwrap();

        let mut output = String::new();
        file.read_to_string(&mut output).unwrap();
        assert_eq!(output, "ONETWO");
    }

    #[tokio::test]
    async fn replay_output_preserves_positioned_file_writes() {
        let mut file = tempfile::tempfile().unwrap();
        emit_replay_output(file.as_raw_fd(), b"X", Some(5), false).await;
        file.rewind().unwrap();

        let mut output = Vec::new();
        file.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"\0\0\0\0\0X");
    }

    #[tokio::test]
    async fn positioned_replay_advances_shared_offset_for_write() {
        let mut file = tempfile::tempfile().unwrap();
        emit_replay_output(file.as_raw_fd(), b"X", Some(5), true).await;
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

        emit_replay_output(fd, b"X", Some(1), false).await;
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
        emit_replay_output(output.as_raw_fd(), b"SOCKET_OUT", None, false).await;

        let mut received = [0; 10];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"SOCKET_OUT");
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
            emit_replay_output(output.as_raw_fd(), &expected, None, false).await;
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
