/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Diagnostic writes that never wait for the reader of a descriptor.
//!
//! While `--max-log-bytes` is in force, termination of a run must not depend
//! on any diagnostic write (review of
//! <https://github.com/rrnewton/hermit/pull/3686>). A plain `write(2)` to a
//! blocking stderr whose reader stopped reading waits forever, so every
//! hermit diagnostic that can run in a capped run before the cap or the 123
//! classification goes through [`write_without_waiting`] once
//! [`forbid_waiting_diagnostics`] has been called.
//!
//! [`crate::util::RetryingStderr`] reads the same flag: while it is set, its
//! writes go through [`write_without_waiting_for_a_reader`] and wait only
//! within its bounded deadline, never inside `write(2)`.
//!
//! This lives in detcore, below the `hermit` library and binary, because
//! detcore's own stderr writer reads the flag, and because `hermit`'s
//! `proc_mount` writes its warning from the namespace-only `pre_exec` callback,
//! after `fork`, and from the container init: every function here except
//! [`write_without_waiting_for_a_reader`] makes only syscalls and signal-set
//! operations, without allocation, stdio locks or formatting.

use std::io;
use std::mem;
use std::os::fd::RawFd;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Set once, before any fork, by the `hermit` binary when `--max-log-bytes`
/// is in force. A forked child (the container init, a `pre_exec` callback)
/// inherits the value with the rest of its memory.
static DIAGNOSTICS_MUST_NOT_WAIT: AtomicBool = AtomicBool::new(false);

/// From now on, in this process and every process it forks, a diagnostic that
/// asks [`diagnostics_must_not_wait`] is written once with
/// [`write_without_waiting`] and omitted when stderr cannot take it at once,
/// and [`crate::util::RetryingStderr`] never waits inside `write(2)`.
/// Only `hermit`'s `main` calls this, when `--max-log-bytes` is given.
pub fn forbid_waiting_diagnostics() {
    DIAGNOSTICS_MUST_NOT_WAIT.store(true, Ordering::Relaxed);
}

/// Whether [`forbid_waiting_diagnostics`] was called. A single atomic load, so
/// it is safe after `fork` and in a signal handler.
pub fn diagnostics_must_not_wait() -> bool {
    DIAGNOSTICS_MUST_NOT_WAIT.load(Ordering::Relaxed)
}

/// Put `bytes` on `fd` once, without waiting for the other end and without
/// letting the attempt raise a signal. A diagnostic that cannot be delivered
/// that way is omitted: the exit status still says why the run ended.
///
/// THE DESCRIPTOR ITSELF IS NEVER MADE NON-BLOCKING. An inherited descriptor's
/// open file description is shared with the parent, the guest or a terminal,
/// and `fcntl(O_NONBLOCK)` on it would leak into all of them. Each kind of file
/// instead gets a primitive that is non-blocking on its own:
///
/// - a regular file: `pwritev2(RWF_NOWAIT)` at offset -1, so the file position
///   and `O_APPEND` are honoured. `EAGAIN` or `EOPNOTSUPP` omit the line.
///   Buffered `RWF_NOWAIT` writes fail with `EAGAIN` on btrfs and with
///   `EOPNOTSUPP` on tmpfs, so a log file there does not get the line.
/// - a socket: `send(MSG_DONTWAIT | MSG_NOSIGNAL)`.
/// - a pipe, FIFO or terminal: a NEW open file description for the same
///   object, opened through `/proc/self/fd/<fd>` with `O_NONBLOCK`, and one
///   write of at most `PIPE_BUF` bytes, which a pipe either takes whole or
///   refuses with `EAGAIN`. Opening a pipe that has no reader fails with
///   `ENXIO` rather than raising `SIGPIPE`.
/// - anything else, including a descriptor whose type cannot be learned:
///   omitted.
///
/// The file type comes from `file_type` in this module.
///
/// No signal escapes; see [`suppressing_diagnostic_signals`].
///
/// What this cannot avoid are short kernel locks that sleep uninterruptibly,
/// which no handled signal or timer could break either: the file-position lock
/// of an open file description another process is writing through at that
/// moment, a terminal's termios and output locks, a socket's lock. None of
/// them waits for a reader to drain anything.
pub fn write_without_waiting(fd: RawFd, bytes: &[u8]) {
    let bytes = &bytes[..bytes.len().min(libc::PIPE_BUF)];
    suppressing_diagnostic_signals(|| {
        let Ok(file_type) = file_type(fd) else {
            return 0;
        };
        let written = match file_type {
            libc::S_IFREG => {
                let iov = libc::iovec {
                    iov_base: bytes.as_ptr() as *mut libc::c_void,
                    iov_len: bytes.len(),
                };
                // SAFETY: one valid iovec over `bytes`; offset -1 is the
                // current position.
                unsafe { libc::pwritev2(fd, &iov, 1, -1, libc::RWF_NOWAIT) }
            }
            // SAFETY: `bytes` is valid for its length.
            libc::S_IFSOCK => unsafe {
                libc::send(
                    fd,
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            },
            libc::S_IFIFO => return write_through_new_description(fd, bytes),
            // SAFETY: isatty only inspects `fd`.
            libc::S_IFCHR if unsafe { libc::isatty(fd) } == 1 => {
                return write_through_new_description(fd, bytes);
            }
            _ => return 0,
        };
        if written < 0 { last_errno() } else { 0 }
    });
}

/// One write of `bytes` to `fd` that cannot wait for a reader to drain the
/// file, for a writer that then waits itself, within its own bound, when it
/// is told the file is full ([`crate::util::RetryingStderr`] while
/// [`diagnostics_must_not_wait`]). Returns the bytes written, which may be
/// fewer than `bytes.len()`, or the error; `WouldBlock` means full.
///
/// THE DESCRIPTOR ITSELF IS NEVER MADE NON-BLOCKING, as in
/// [`write_without_waiting`]:
///
/// - a pipe, a FIFO or any character device, a terminal or `/dev/null`: one
///   write through a new `O_NONBLOCK` open file description for the same
///   object (see [`write_without_waiting`]). Opening a pipe that has no reader
///   fails with `ENXIO`, reported here as `EPIPE`, what a write to it would
///   have returned. If the new description cannot be opened, nothing is
///   written and the error is returned.
///
///   ⚠️ NO CHARACTER DEVICE GETS A PLAIN WRITE, not even one the terminal
///   query says is not a terminal. Nothing the process asks can tell a
///   terminal from `/dev/null` in a way a seccomp policy cannot forge: a
///   policy that answers the query with `ENOTTY` makes a terminal look like
///   `/dev/null`, and a plain write to a full terminal waits for a reader
///   forever.
/// - a socket: `send(MSG_DONTWAIT | MSG_NOSIGNAL)`.
/// - anything else, a regular file included: a plain `write(2)`. Nothing
///   reads a regular file, so the write cannot wait for a reader to drain it;
///   and `RWF_NOWAIT` would refuse buffered writes on btrfs and tmpfs, which
///   would lose every line of a log written there. A regular file on a
///   network or FUSE file system whose server has stopped answering can
///   still hold this write.
/// - a descriptor whose type cannot be learned (see `file_type` in this
///   module): nothing is written, and the error is returned, so the caller
///   gives up on the bytes.
///
/// No `SIGPIPE`, `SIGXFSZ` or `SIGTTOU` escapes; see
/// [`suppressing_diagnostic_signals`].
pub fn write_without_waiting_for_a_reader(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // Replaced by the closure; left only if the signal mask could not be set,
    // in which case nothing was written.
    let mut result = Err(io::Error::other(
        "the signal mask for a diagnostic write could not be set",
    ));
    suppressing_diagnostic_signals(|| {
        let file_type = match file_type(fd) {
            Ok(file_type) => file_type,
            Err(errno) => {
                // ⚠️ FAIL CLOSED. Without the file type there is no way to
                // tell a pipe from a regular file, and a plain write to a full
                // pipe waits forever. The line is not written; the caller
                // gives up on it.
                result = Err(io::Error::from_raw_os_error(errno));
                return 0;
            }
        };
        let written = match file_type {
            // SAFETY: `bytes` is valid for its length.
            libc::S_IFSOCK => unsafe {
                libc::send(
                    fd,
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            },
            libc::S_IFIFO | libc::S_IFCHR => {
                let path = proc_self_fd_path(fd);
                // SAFETY: `path` is NUL-terminated.
                let reopened = unsafe {
                    libc::open(
                        path.as_ptr().cast(),
                        libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY,
                    )
                };
                if reopened < 0 {
                    let errno = match last_errno() {
                        libc::ENXIO => libc::EPIPE,
                        errno => errno,
                    };
                    result = Err(io::Error::from_raw_os_error(errno));
                    // Nothing was written, so no signal was generated.
                    return 0;
                }
                // SAFETY: `bytes` is valid for its length; `reopened` is ours.
                let written = unsafe { libc::write(reopened, bytes.as_ptr().cast(), bytes.len()) };
                let errno = last_errno();
                // SAFETY: closes only the descriptor opened above.
                unsafe { libc::close(reopened) };
                if written < 0 {
                    result = Err(io::Error::from_raw_os_error(errno));
                    return errno;
                }
                written
            }
            // SAFETY: `bytes` is valid for its length.
            _ => unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) },
        };
        if written < 0 {
            let error = io::Error::last_os_error();
            let errno = error.raw_os_error().unwrap_or(0);
            result = Err(error);
            errno
        } else {
            result = Ok(written as usize);
            0
        }
    });
    result
}

/// The type of the file behind `fd`, its nonzero `S_IFMT` bits, or the errno
/// that kept it from being learned.
///
/// `statx(AT_STATX_DONT_SYNC)` first: it answers from cached attributes
/// instead of asking a network or FUSE file system. When `statx` fails (a
/// seccomp policy that denies it, as the Rust standard library also allows
/// for) or names no type, two questions that the open file answers from
/// memory: `fcntl(F_GETPIPE_SZ)`, which only a pipe or a FIFO answers, and
/// `getsockopt(SO_TYPE)`, which only a socket answers. Any other file then
/// has no type here, and the writers fail closed. The error is `statx`'s, or
/// `EIO` when `statx` returned 0 without a type.
///
/// ⚠️ NO `fstat`. It asks a network or FUSE file system for attributes once
/// its cached ones have expired, and waits for the server's answer; a server
/// that has stopped answering, behind a FIFO or a regular file on stderr,
/// would then hold the crossing writer before `_exit(123)`, and the stderr
/// log sink before the cap is crossed.
///
/// ⚠️ A CALL THAT RETURNS 0 BUT NAMES NO TYPE HAS FAILED. A seccomp policy
/// that answers with errno 0 makes the call return 0 and fill in nothing, and
/// a zeroed type would read as "anything else", which
/// [`write_without_waiting_for_a_reader`] writes to plainly. For the same
/// reason a pipe size of 0 and a socket type of 0 are not answers: no pipe
/// holds 0 bytes, and no socket type is 0.
fn file_type(fd: RawFd) -> Result<libc::mode_t, i32> {
    // SAFETY: statx writes only into the zeroed struct it is given; an empty
    // path with AT_EMPTY_PATH names `fd` itself.
    let mut stat: reverie::syscalls::linux_abi::Statx = unsafe { mem::zeroed() };
    let inspected = unsafe {
        reverie::syscalls::linux_abi::statx(
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | reverie::syscalls::linux_abi::AT_STATX_DONT_SYNC,
            reverie::syscalls::linux_abi::STATX_TYPE,
            &mut stat,
        )
    } == 0;
    let statx_errno = if inspected { libc::EIO } else { last_errno() };
    let file_type = libc::mode_t::from(stat.stx_mode) & libc::S_IFMT;
    if inspected && stat.stx_mask & reverie::syscalls::linux_abi::STATX_TYPE != 0 && file_type != 0
    {
        return Ok(file_type);
    }
    // SAFETY: F_GETPIPE_SZ only reads the pipe behind `fd`.
    if unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) } > 0 {
        return Ok(libc::S_IFIFO);
    }
    let mut socket_type: libc::c_int = 0;
    let mut length = mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `length` bytes into `socket_type`.
    let answered = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&raw mut socket_type).cast(),
            &mut length,
        )
    } == 0;
    if answered && socket_type != 0 {
        return Ok(libc::S_IFSOCK);
    }
    Err(statx_errno)
}

/// The calling thread's `errno`, 0 when there is none.
pub fn last_errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Write `bytes` through a new, non-blocking open file description for the
/// pipe, FIFO or terminal behind `fd`. Returns the write's errno, 0 for none.
/// `O_NOCTTY`: a session leader (the container init) must not acquire the
/// terminal as its controlling terminal by writing a diagnostic to it.
fn write_through_new_description(fd: RawFd, bytes: &[u8]) -> i32 {
    let path = proc_self_fd_path(fd);
    // SAFETY: `path` is NUL-terminated.
    let reopened = unsafe {
        libc::open(
            path.as_ptr().cast(),
            libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY,
        )
    };
    if reopened < 0 {
        // ENXIO is a pipe with no reader: nothing written, nothing raised.
        return 0;
    }
    // SAFETY: `bytes` is valid for its length; `reopened` is ours.
    let written = unsafe { libc::write(reopened, bytes.as_ptr().cast(), bytes.len()) };
    let errno = if written < 0 { last_errno() } else { 0 };
    // SAFETY: closes only the descriptor opened above.
    unsafe { libc::close(reopened) };
    errno
}

/// `/proc/self/fd/<fd>` as a NUL-terminated path, built without allocating:
/// the crossing writer may run with arbitrary locks held.
pub fn proc_self_fd_path(fd: RawFd) -> [u8; 32] {
    const PREFIX: &[u8] = b"/proc/self/fd/";
    let mut path = [0u8; 32];
    path[..PREFIX.len()].copy_from_slice(PREFIX);
    let mut digits = [0u8; 10];
    let mut count = 0;
    let mut rest = fd.unsigned_abs();
    loop {
        digits[count] = b'0' + (rest % 10) as u8;
        count += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    for (slot, digit) in path[PREFIX.len()..]
        .iter_mut()
        .zip(digits[..count].iter().rev())
    {
        *slot = *digit;
    }
    path
}

/// Run one diagnostic write (`write` returns its errno, 0 for none) so that no
/// signal it generates escapes.
///
/// Reverie's container setup restores `SIGPIPE`'s default disposition and
/// clears the signal mask in the process that runs the tracer. Under
/// `--no-namespace` that was an ordinary process, and round 2 of the review of
/// <https://github.com/rrnewton/hermit/pull/3686> found a `SIGPIPE` from its
/// crossing line killing it before `_exit(123)`, reported as an internal
/// failure (125); the cap is refused there now. In hermit's PID namespace the
/// tracer runs in the container init, PID 1 of the namespace, and the kernel
/// discards a signal at its default disposition that the init raises in
/// itself, so there the guard is a defence rather than the fix: it keeps any
/// process in which this writer runs with `SIGPIPE` at its default from dying
/// before the exit.
/// As in `proc_mount`'s warning writer, `SIGPIPE` and `SIGXFSZ` (a file-size
/// limit) are blocked for the attempt, and a signal is consumed only when this
/// write failed with the matching error and the signal was not already pending
/// before it. `SIGTTOU` is blocked too: a terminal with `TOSTOP` treats a
/// blocked `SIGTTOU` as ignored and accepts the write, instead of stopping a
/// background process. The original mask is restored afterwards.
pub fn suppressing_diagnostic_signals(write: impl FnOnce() -> i32) {
    // SAFETY: sigset operations on local, initialized sets; pthread_sigmask and
    // sigtimedwait affect only the calling thread.
    unsafe {
        let mut blocked: libc::sigset_t = mem::zeroed();
        libc::sigemptyset(&mut blocked);
        for signal in [libc::SIGPIPE, libc::SIGXFSZ, libc::SIGTTOU] {
            libc::sigaddset(&mut blocked, signal);
        }
        let mut original: libc::sigset_t = mem::zeroed();
        if libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut original) != 0 {
            return;
        }
        let mut pending_before: libc::sigset_t = mem::zeroed();
        libc::sigemptyset(&mut pending_before);
        libc::sigpending(&mut pending_before);
        let errno = write();
        let generated = match errno {
            libc::EPIPE => Some(libc::SIGPIPE),
            libc::EFBIG => Some(libc::SIGXFSZ),
            _ => None,
        };
        if let Some(signal) = generated
            && libc::sigismember(&pending_before, signal) == 0
        {
            let mut consume: libc::sigset_t = mem::zeroed();
            libc::sigemptyset(&mut consume);
            libc::sigaddset(&mut consume, signal);
            let now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            while libc::sigtimedwait(&consume, std::ptr::null_mut(), &now) < 0
                && last_errno() == libc::EINTR
            {}
        }
        libc::pthread_sigmask(libc::SIG_SETMASK, &original, std::ptr::null_mut());
    }
}
