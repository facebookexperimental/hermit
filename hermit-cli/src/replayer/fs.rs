/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::fd::AsRawFd;
use std::sync::Mutex;

use reverie::Errno;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
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
use reverie::syscalls::Timespec;
use reverie::syscalls::family::StatFamily;
use reverie::syscalls::family::WriteFamily;
use reverie::syscalls::ioctl;

use super::Replayer;
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

static REPLAY_OUTPUT_LOCK: Mutex<()> = Mutex::new(());

fn emit_replay_output(
    output_fd: libc::c_int,
    bytes: &[u8],
    file_offset: Option<i64>,
    advances_output_offset: bool,
) {
    if bytes.is_empty() {
        return;
    }

    let _guard = REPLAY_OUTPUT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: fcntl only inspects or changes the status flags of this valid,
    // inherited output descriptor.
    let flags = unsafe { libc::fcntl(output_fd, libc::F_GETFL) };
    if flags == -1 {
        tracing::debug!(
            error = %std::io::Error::last_os_error(),
            output_fd,
            "could not inspect replay output"
        );
        return;
    }
    let mut temporary_flags = flags | libc::O_NONBLOCK;
    if file_offset.is_some() {
        temporary_flags &= !libc::O_APPEND;
    }
    let changed_flags = temporary_flags != flags;
    if changed_flags {
        // SAFETY: the descriptor remains open for the duration of this call.
        if unsafe { libc::fcntl(output_fd, libc::F_SETFL, temporary_flags) } == -1 {
            tracing::debug!(
                error = %std::io::Error::last_os_error(),
                output_fd,
                "could not make replay output nonblocking"
            );
            return;
        }
    }

    let mut offset = 0;
    while offset < bytes.len() {
        let remaining = &bytes[offset..];
        let written = if let Some(file_offset) = file_offset {
            let position = file_offset
                .checked_add(offset as i64)
                .expect("recorded output offset overflow");
            // SAFETY: remaining points to readable memory and output_fd is open.
            unsafe {
                libc::pwrite(
                    output_fd,
                    remaining.as_ptr().cast(),
                    remaining.len(),
                    position,
                )
            }
        } else {
            // send with MSG_NOSIGNAL handles sockets without risking a tracer
            // SIGPIPE. Pipes reject send with ENOTSOCK, so use their
            // now-nonblocking write path instead.
            let sent = unsafe {
                libc::send(
                    output_fd,
                    remaining.as_ptr().cast(),
                    remaining.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if sent == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOTSOCK)
            {
                // SAFETY: remaining points to readable memory and output_fd is open.
                unsafe { libc::write(output_fd, remaining.as_ptr().cast(), remaining.len()) }
            } else {
                sent
            }
        };
        if written > 0 {
            offset += written as usize;
            continue;
        }
        if written == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            tracing::debug!(%error, output_fd, "could not emit all replay output");
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
    if changed_flags {
        // SAFETY: restore the descriptor status flags before releasing the lock.
        if unsafe { libc::fcntl(output_fd, libc::F_SETFL, flags) } == -1 {
            tracing::debug!(
                error = %std::io::Error::last_os_error(),
                output_fd,
                "could not restore replay output flags"
            );
        }
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

impl Replayer {
    /// Replays the vectored read family (`readv`/`preadv`/`preadv2`) by
    /// scattering the recorded flattened output bytes across the guest's current
    /// `iovec` buffers, without touching any live descriptor.
    pub(super) async fn handle_readv_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        iov_addr: Option<usize>,
        iovcnt: usize,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, ReadvV2)?;
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

    fn replay_output<G: Guest<Self>>(
        &self,
        guest: &mut G,
        advances_output_offset: bool,
        syscall: WriteFamily,
        output_fd: i32,
        count: usize,
        output_offset: Option<i64>,
    ) -> Result<(), Errno> {
        let bytes = read_write_bytes(&guest.memory(), syscall, count)?;
        let output = self.output_endpoint(output_fd);
        emit_replay_output(output, &bytes, output_offset, advances_output_offset);
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
            )?;
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
            ioctl::Request::FIOCLEX | ioctl::Request::FIONCLEX | ioctl::Request::FIONBIO(_)
        ) {
            // Replayed opens do not necessarily create host file descriptors.
            // Detcore updates the logical descriptor metadata after this returns.
            next_event!(guest, Return)
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
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn replay_output_preserves_regular_file_offset() {
        let mut file = tempfile::tempfile().unwrap();
        emit_replay_output(file.as_raw_fd(), b"ONE", None, false);
        emit_replay_output(file.as_raw_fd(), b"TWO", None, false);
        file.rewind().unwrap();

        let mut output = String::new();
        file.read_to_string(&mut output).unwrap();
        assert_eq!(output, "ONETWO");
    }

    #[test]
    fn replay_output_preserves_positioned_file_writes() {
        let mut file = tempfile::tempfile().unwrap();
        emit_replay_output(file.as_raw_fd(), b"X", Some(5), false);
        file.rewind().unwrap();

        let mut output = Vec::new();
        file.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"\0\0\0\0\0X");
    }

    #[test]
    fn positioned_replay_advances_shared_offset_for_write() {
        let mut file = tempfile::tempfile().unwrap();
        emit_replay_output(file.as_raw_fd(), b"X", Some(5), true);
        assert_eq!(file.stream_position().unwrap(), 6);
        file.rewind().unwrap();

        let mut output = Vec::new();
        file.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"\0\0\0\0\0X");
    }
    #[test]
    fn positioned_replay_temporarily_clears_append() {
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

        emit_replay_output(fd, b"X", Some(1), false);
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
    #[test]
    fn replay_output_supports_sockets() {
        let (output, mut peer) = UnixStream::pair().unwrap();
        emit_replay_output(output.as_raw_fd(), b"SOCKET_OUT", None, false);

        let mut received = [0; 10];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"SOCKET_OUT");
    }

    #[test]
    fn replay_output_does_not_block_on_full_pipe() {
        let mut pipe = [0; 2];
        // SAFETY: pipe points to two writable integers.
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );

        let fill = [0u8; 4096];
        loop {
            // SAFETY: pipe[1] is open and fill is readable.
            let written = unsafe { libc::write(pipe[1], fill.as_ptr().cast(), fill.len()) };
            if written >= 0 {
                continue;
            }
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EAGAIN)
            );
            break;
        }

        // SAFETY: pipe[1] is open and F_GETFL does not mutate memory.
        let flags = unsafe { libc::fcntl(pipe[1], libc::F_GETFL) };
        assert_ne!(flags, -1);
        // SAFETY: pipe[1] is open; clearing O_NONBLOCK models a blocking sink.
        assert_ne!(
            unsafe { libc::fcntl(pipe[1], libc::F_SETFL, flags & !libc::O_NONBLOCK) },
            -1
        );

        let (finished_tx, finished_rx) = mpsc::channel();
        let write_fd = pipe[1];
        let writer = std::thread::spawn(move || {
            emit_replay_output(write_fd, b"x", None, false);
            finished_tx.send(()).unwrap();
        });
        let result = finished_rx.recv_timeout(Duration::from_secs(1));
        if result.is_err() {
            // SAFETY: closing the read end releases a mistakenly blocked writer.
            unsafe { libc::close(pipe[0]) };
        }
        writer.join().unwrap();
        assert!(result.is_ok(), "replay output blocked on a full pipe");

        if result.is_ok() {
            // SAFETY: the read end is still open on the successful path.
            unsafe { libc::close(pipe[0]) };
        }
        // SAFETY: the write end remains open until after the worker exits.
        unsafe { libc::close(pipe[1]) };
    }
}
