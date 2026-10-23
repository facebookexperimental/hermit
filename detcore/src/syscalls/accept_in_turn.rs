/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A blocking `accept(2)` that record/replay runs in the caller's turn
//! (<https://github.com/rrnewton/hermit/issues/3880>).
//!
//! Backgrounded, an accept lets the kernel choose the new descriptor's number
//! at a host-timed moment relative to other threads' `open` and `close`, which
//! replay cannot reproduce. Here every attempt is the guest's own `accept4`,
//! run nonblocking inside the caller's turn and recorded, so the number is
//! allocated in a deterministic turn and replay serves the same sequence of
//! attempts. Between attempts the thread yields its turn, as a nonblockized
//! read on a container-internal pipe does.
//!
//! Under record, the listener is made nonblocking for one attempt at a time
//! through Detcore's own copy of it ([`AcceptBracket`]). Replay serves every
//! attempt from the log and runs no bracket.

use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::Errno;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use crate::fd::ReceiveTimeout;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::helpers::record_retry_event;
use crate::tool_global::ResumeStatus;
use crate::tool_global::resource_request;
use crate::tool_global::thread_observe_time;
use crate::tool_local::Detcore;
use crate::tool_local::FileMetadata;

/// Record's refusal of a `close`, `dup2`/`dup3` or `close_range` that would
/// close a listener a blocking accept still waits on (Linux would keep
/// waiting on the old listener).
pub(crate) const ACCEPT_CLOSE_REFUSAL: &str = "hermit refused the recording: a descriptor was \
    closed while a blocking accept still waits on its listener";

/// Record's refusal of an accept whose listener was passed over `SCM_RIGHTS`
/// while it waited.
const ACCEPT_EXPORT_REFUSAL: &str = "hermit refused the recording: a blocking accept's \
    listener was passed over SCM_RIGHTS while the accept waited";

/// Record's refusal when the listener's file status flags could not be
/// restored after an attempt.
const ACCEPT_RESTORE_REFUSAL: &str = "hermit refused the recording: an accept could not \
    restore its listener's file status flags";

/// Record's refusal when Detcore could not copy the listener or make it
/// nonblocking for an attempt.
const ACCEPT_SET_REFUSAL: &str = "hermit refused the recording: an accept could not make \
    its listener nonblocking for one attempt";

/// Record's refusal of an accept whose listener's receive timeout Detcore
/// does not know: set with a value it could not read, or, as the kernel's
/// value shows, through a copy of the listener Detcore does not track as an
/// alias.
const ACCEPT_TIMEOUT_REFUSAL: &str = "hermit refused the recording: a blocking accept's \
    listener has a receive timeout (SO_RCVTIMEO) that Hermit did not see set";

/// What Detcore prints when the fault hook kills the accepting task.
const ACCEPT_BRACKET_KILL_NOTICE: &str =
    "accept bracket fault: killed the accepting task after the temporary O_NONBLOCK";

/// Test-only fault hook, like `HERMIT_TEST_CONTAINER_CHILD_FAULT`: with
/// `kill-after-set`, Detcore kills the accepting task at the first attempt of
/// the first blocking accept, after the temporary `O_NONBLOCK` (in replay, at
/// the same point, which has no bracket); with `restore-fails`, that attempt's
/// restore reports a failure. Unset, nothing changes.
const FAULT_ENV: &str = "HERMIT_TEST_ACCEPT_BRACKET_FAULT";

/// Whether the fault hook has fired in this process. It fires once.
static FAULT_FIRED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BracketFault {
    KillAfterSet,
    RestoreFails,
}

/// The fault the hook asks for at this attempt, at most once per process.
fn take_bracket_fault() -> Option<BracketFault> {
    let fault = match std::env::var(FAULT_ENV).as_deref() {
        Ok("kill-after-set") => BracketFault::KillAfterSet,
        Ok("restore-fails") => BracketFault::RestoreFails,
        _ => return None,
    };
    (!FAULT_FIRED.swap(true, Ordering::SeqCst)).then_some(fault)
}

/// A tracer-side copy of descriptor `fd` in the table of guest thread `tid`
/// of process `pid`: the same open file description, held in no guest table.
/// A thread pidfd (`PIDFD_THREAD`, Linux 6.9 and later) reads that thread's
/// own table, which a thread made without `CLONE_FILES` does not share with
/// its leader. Where Linux has no thread pidfd, the copy comes through the
/// leader's pidfd only if `KCMP_FILES` shows the two share one table, as
/// `hermit-cli`'s `duplicate_guest_thread_fd` does; otherwise it fails with
/// `EINVAL`.
fn tracer_copy(pid: libc::pid_t, tid: libc::pid_t, fd: RawFd) -> io::Result<OwnedFd> {
    const PIDFD_THREAD: libc::c_uint = libc::O_EXCL as libc::c_uint;
    let pidfd = if pid == tid {
        open_pidfd(pid, 0)?
    } else {
        match open_pidfd(tid, PIDFD_THREAD) {
            Ok(pidfd) => pidfd,
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
                if !shares_fd_table(pid, tid)? {
                    return Err(error);
                }
                open_pidfd(pid, 0)?
            }
            Err(error) => return Err(error),
        }
    };
    // SAFETY: pidfd_getfd takes no pointers. Its result is close-on-exec.
    let copy = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), fd, 0) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pidfd_getfd returned a new descriptor owned by this process.
    Ok(unsafe { OwnedFd::from_raw_fd(copy as RawFd) })
}

fn open_pidfd(pid: libc::pid_t, flags: libc::c_uint) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes no pointers.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, flags) };
    if pidfd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pidfd_open returned a new descriptor owned by this process.
    Ok(unsafe { OwnedFd::from_raw_fd(pidfd as RawFd) })
}

/// Whether tasks `left` and `right` share one descriptor table.
fn shares_fd_table(left: libc::pid_t, right: libc::pid_t) -> io::Result<bool> {
    const KCMP_FILES: libc::c_int = 2;
    // SAFETY: kcmp(KCMP_FILES) takes no pointers.
    let comparison = unsafe { libc::syscall(libc::SYS_kcmp, left, right, KCMP_FILES, 0, 0) };
    if comparison < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(comparison == 0)
}

/// The kernel's receive timeout on `socket`, as `getsockopt(SO_RCVTIMEO)`
/// reads it: rounded up to the scheduler tick, and zero for both no timeout
/// and an immediate one.
fn kernel_receive_timeout(socket: BorrowedFd<'_>) -> io::Result<Duration> {
    let mut value = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut length = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
    // SAFETY: both pointers name locals of the sizes passed.
    if unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw mut value).cast(),
            &mut length,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Duration::from_secs(value.tv_sec.max(0) as u64)
        + Duration::from_micros(value.tv_usec.max(0) as u64))
}

/// Whether the kernel's receive timeout `kernel` (`kernel_receive_timeout`)
/// is what setting `modeled` leaves. Linux rounds a timeout up to its tick, at
/// most 10 ms (`HZ` is at least 100). (A timeout too long to count in ticks is
/// modeled as `None`; see `ReceiveTimeout::set_by`.)
fn receive_timeout_agrees(modeled: ReceiveTimeout, kernel: Duration) -> bool {
    const TICK: Duration = Duration::from_millis(10);
    match modeled {
        ReceiveTimeout::None | ReceiveTimeout::Immediate => kernel.is_zero(),
        ReceiveTimeout::After(timeout) => kernel >= timeout && kernel - timeout <= TICK,
        ReceiveTimeout::Unknown => false,
    }
}

/// Owns the temporary `O_NONBLOCK` on a listener's open file description for
/// one accept attempt. It sets and restores the flags through Detcore's own
/// copy of the listener, never through a guest descriptor number, which
/// another thread could have reused.
///
/// It restores the flags it read at this attempt, so an `F_SETFL` that a
/// sibling made while the accept waited stands. [`AcceptBracket::restore`]
/// reports a failure; if the bracket is dropped first (the handler returned
/// early, or its future was dropped because the task died), the drop restores
/// the flags before any other container thread runs, and a failure there ends
/// the run with the same refusal (`exit_on_scheduler_refusal`).
// TODO-HUMAN-REVIEW(PR-3908)
pub(crate) struct AcceptBracket<'copy> {
    copy: BorrowedFd<'copy>,
    flags: libc::c_int,
    restored: bool,
}

impl<'copy> AcceptBracket<'copy> {
    /// Read the description's file status flags and add `O_NONBLOCK`.
    pub(crate) fn set(copy: BorrowedFd<'copy>) -> io::Result<Self> {
        // SAFETY: fcntl on a descriptor this process owns.
        let flags = unsafe { libc::fcntl(copy.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        if unsafe { libc::fcntl(copy.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            copy,
            flags,
            restored: false,
        })
    }

    /// Put back the flags read by [`AcceptBracket::set`].
    pub(crate) fn restore(mut self) -> io::Result<()> {
        self.restore_flags()
    }

    fn restore_flags(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        // SAFETY: fcntl on a descriptor this process owns.
        if unsafe { libc::fcntl(self.copy.as_raw_fd(), libc::F_SETFL, self.flags) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.restored = true;
        Ok(())
    }
}

impl Drop for AcceptBracket<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.restore_flags() {
            tracing::error!("[detcore] {} ({})", ACCEPT_RESTORE_REFUSAL, error);
            crate::scheduler::exit_on_scheduler_refusal(&ACCEPT_RESTORE_REFUSAL);
        }
    }
}

/// One waiting accept's entry among its descriptor table's accept guards
/// (`FileMetadata::add_accept_wait`). While it lives, a `close`, `dup2`/`dup3`
/// or `close_range` that would close its listener in that table is refused.
/// It holds the table it registered in, so its drop removes exactly its own
/// entry there, whichever way the accept ends, even after the task's own
/// table changed (`CLONE_FILES` sharers exec into tables of their own).
// TODO-HUMAN-REVIEW(PR-3908)
struct AcceptWait {
    table: Arc<Mutex<FileMetadata>>,
    operation: u64,
}

impl AcceptWait {
    fn register(table: &Arc<Mutex<FileMetadata>>, listener: RawFd) -> Self {
        let operation = table.lock().unwrap().add_accept_wait(listener);
        Self {
            table: Arc::clone(table),
            operation,
        }
    }
}

impl Drop for AcceptWait {
    fn drop(&mut self) {
        if let Ok(mut table) = self.table.lock() {
            table.remove_accept_wait(self.operation);
        }
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Run the guest's `accept4` on a listener eligible for accept in turn
    /// (`DetFd::is_accept_in_turn_listener`).
    ///
    /// A listener the guest made nonblocking (Detcore's logical flag, which
    /// both phases evolve alike) gets one plain attempt, and one with an
    /// immediate receive timeout one attempt in this turn. Otherwise the
    /// accept takes the listener's
    /// `SO_RCVTIMEO` from Detcore's model (`DetFd::receive_timeout`), which
    /// record first checks against the kernel's value, and retries one
    /// nonblocking attempt per turn until an attempt does not report `EAGAIN`.
    /// It ends with `EAGAIN` at the virtual deadline, and on a signal with
    /// `EINTR` when timed and `ERESTARTSYS` otherwise, which is the mapping of
    /// Linux's `sock_intr_errno`, so `SA_RESTART` restarts only an untimed
    /// accept. Unlike Linux, any signal that reaches the thread ends the wait,
    /// including one whose disposition ignores it (a default-ignored
    /// `SIGCHLD`, say), so a timed accept can end with a spurious `EINTR`.
    /// Neither a timeout nor a signal makes another attempt.
    // TODO-HUMAN-REVIEW(PR-3908)
    pub(crate) async fn accept_in_turn<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Accept4,
    ) -> Result<i64, Error> {
        let listener = call.sockfd();
        if guest
            .thread_state()
            .with_detfd(listener, |detfd| detfd.is_nonblocking())?
        {
            return self
                .record_or_replay_preserving_tool_errors(guest, call)
                .await;
        }
        let copy = if self.cfg.replaying {
            None
        } else {
            match tracer_copy(guest.pid().as_raw(), guest.tid().as_raw(), listener) {
                Ok(copy) => Some(copy),
                Err(error) => {
                    return self
                        .refuse_accept_in_turn(guest, Sysno::accept4, ACCEPT_SET_REFUSAL, &error)
                        .await;
                }
            }
        };
        let modeled = guest
            .thread_state()
            .with_detfd(listener, |detfd| detfd.receive_timeout())?;
        // Under record, the kernel's value shows a timeout set through a copy
        // of the listener that the model does not track as an alias.
        let kernel = copy
            .as_ref()
            .map(|copy| kernel_receive_timeout(copy.as_fd()))
            .transpose();
        let timeout = match (modeled, kernel) {
            (ReceiveTimeout::Unknown, _) => {
                let cause = "set with a value Hermit could not read";
                return self
                    .refuse_accept_in_turn(guest, Sysno::accept4, ACCEPT_TIMEOUT_REFUSAL, &cause)
                    .await;
            }
            (_, Err(error)) => {
                return self
                    .refuse_accept_in_turn(guest, Sysno::accept4, ACCEPT_TIMEOUT_REFUSAL, &error)
                    .await;
            }
            (modeled, Ok(Some(kernel))) if !receive_timeout_agrees(modeled, kernel) => {
                let cause = format!("modeled {modeled:?}, kernel {kernel:?}");
                return self
                    .refuse_accept_in_turn(guest, Sysno::accept4, ACCEPT_TIMEOUT_REFUSAL, &cause)
                    .await;
            }
            (ReceiveTimeout::Immediate, _) => {
                // Linux fails at once with EAGAIN when nothing is queued: one
                // attempt in this turn, bracketed under record so that it
                // cannot block even if the kernel's zero reading meant none.
                let bracket = match &copy {
                    Some(copy) => match AcceptBracket::set(copy.as_fd()) {
                        Ok(bracket) => Some(bracket),
                        Err(error) => {
                            return self
                                .refuse_accept_in_turn(
                                    guest,
                                    Sysno::accept4,
                                    ACCEPT_SET_REFUSAL,
                                    &error,
                                )
                                .await;
                        }
                    },
                    None => None,
                };
                let attempt = self
                    .record_or_replay_preserving_tool_errors(guest, call)
                    .await;
                if let Some(Err(error)) = bracket.map(AcceptBracket::restore) {
                    return self
                        .refuse_accept_in_turn(
                            guest,
                            Sysno::accept4,
                            ACCEPT_RESTORE_REFUSAL,
                            &error,
                        )
                        .await;
                }
                return attempt;
            }
            (ReceiveTimeout::After(timeout), _) => Some(timeout),
            (ReceiveTimeout::None, _) => None,
        };
        let deadline = match timeout {
            Some(timeout) => Some(thread_observe_time(guest).await + timeout),
            None => None,
        };
        let table = Arc::clone(&guest.thread_state().file_metadata);
        let _wait = AcceptWait::register(&table, listener);

        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi(call.name());
        loop {
            if matches!(
                resource_request(guest, rsrc.clone()).await,
                ResumeStatus::Signaled(_)
            ) {
                let errno = if timeout.is_some() {
                    Errno::EINTR
                } else {
                    Errno::ERESTARTSYS
                };
                return Err(errno.into());
            }
            // Checked before every bracket, attempt 0 included: the request
            // above yields the turn even then, so a sibling can pass the
            // listener over SCM_RIGHTS after `handle_accept4` found it
            // eligible.
            if guest
                .thread_state()
                .with_detfd(listener, |detfd| detfd.is_exported())?
            {
                let error = io::Error::other("listener exported");
                return self
                    .refuse_accept_in_turn(guest, Sysno::accept4, ACCEPT_EXPORT_REFUSAL, &error)
                    .await;
            }
            let fault = if rsrc.poll_attempt == 0 {
                take_bracket_fault()
            } else {
                None
            };
            let bracket = match &copy {
                Some(copy) => match AcceptBracket::set(copy.as_fd()) {
                    Ok(bracket) => Some(bracket),
                    Err(error) => {
                        return self
                            .refuse_accept_in_turn(
                                guest,
                                Sysno::accept4,
                                ACCEPT_SET_REFUSAL,
                                &error,
                            )
                            .await;
                    }
                },
                None => None,
            };
            if fault == Some(BracketFault::KillAfterSet) {
                // Neither phase runs the attempt: the task is gone, and replay
                // must not consume an event that record never wrote. The
                // bracket's drop restores the flags.
                use std::io::Write as _;
                let _ = writeln!(crate::util::RetryingStderr, "{ACCEPT_BRACKET_KILL_NOTICE}");
                // SAFETY: tgkill takes no pointers.
                unsafe {
                    libc::syscall(
                        libc::SYS_tgkill,
                        guest.pid().as_raw(),
                        guest.tid().as_raw(),
                        libc::SIGKILL,
                    )
                };
                drop(bracket);
                return Err(Errno::EINTR.into());
            }
            let attempt = self
                .record_or_replay_preserving_tool_errors(guest, call)
                .await;
            if let Some(bracket) = bracket {
                let restored = if fault == Some(BracketFault::RestoreFails) {
                    drop(bracket);
                    Err(io::Error::other(format!("injected by {FAULT_ENV}")))
                } else {
                    bracket.restore()
                };
                if let Err(error) = restored {
                    return self
                        .refuse_accept_in_turn(
                            guest,
                            Sysno::accept4,
                            ACCEPT_RESTORE_REFUSAL,
                            &error,
                        )
                        .await;
                }
            }
            let result = match attempt {
                Ok(value) => Ok(value),
                Err(Error::Errno(errno)) => Err(errno),
                Err(error) => return Err(error),
            };
            match result {
                Err(Errno::EAGAIN) => {}
                // A signal interrupted the attempt itself: end as a signal at
                // the wait does. Replay sees the recorded result and maps it
                // alike.
                Err(Errno::ERESTARTSYS) if timeout.is_some() => {
                    return Err(Errno::EINTR.into());
                }
                result => return result.map_err(Error::from),
            }
            rsrc.poll_attempt += 1;
            if let Some(deadline) = deadline
                && thread_observe_time(guest).await >= deadline
            {
                return Err(Errno::EAGAIN.into());
            }
            record_retry_event(guest, call).await;
        }
    }

    /// Refuse the run by name for an accept-in-turn case Detcore cannot serve
    /// faithfully, through the unsupported-operation policy.
    pub(crate) async fn refuse_accept_in_turn<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        refusal: &str,
        cause: &(dyn std::fmt::Display + Sync),
    ) -> Result<i64, Error> {
        use std::io::Write as _;
        tracing::error!("[tid {}] {} ({})", guest.tid(), refusal, cause);
        if self.cfg.panic_on_unsupported_syscalls {
            let _ = writeln!(crate::util::RetryingStderr, "{refusal}");
        }
        self.refuse_unserviceable_operation(guest, sysno, Errno::EOPNOTSUPP)
            .await
    }

    /// Refuse a call that would close `fds` in the caller's descriptor table
    /// while a blocking accept waits on one of them (`AcceptWait`).
    pub(crate) async fn refuse_close_of_waited_listener<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        fds: std::ops::RangeInclusive<u32>,
    ) -> Option<Result<i64, Error>> {
        let waited = guest
            .thread_state()
            .file_metadata
            .lock()
            .unwrap()
            .waited_listener_in(fds)?;
        let cause = format!("descriptor {waited}");
        Some(
            self.refuse_accept_in_turn(guest, sysno, ACCEPT_CLOSE_REFUSAL, &cause)
                .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonblocking(fd: BorrowedFd<'_>) -> bool {
        // SAFETY: fcntl on a descriptor this test owns.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0, "F_GETFL failed: {}", io::Error::last_os_error());
        flags & libc::O_NONBLOCK != 0
    }

    fn stream_socket() -> OwnedFd {
        // SAFETY: socket takes no pointers.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(fd >= 0, "socket failed: {}", io::Error::last_os_error());
        // SAFETY: socket returned a new descriptor owned by this test.
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    /// The bracket's drop puts back the flags it read, through its copy of the
    /// description, so an alias sees them; an explicit restore does the same.
    #[test]
    fn accept_bracket_guard_restores_on_drop() {
        let socket = stream_socket();
        let copy = socket.try_clone().expect("dup failed");
        {
            let _bracket = AcceptBracket::set(copy.as_fd()).expect("set failed");
            assert!(nonblocking(socket.as_fd()), "set did not reach the alias");
        }
        assert!(!nonblocking(socket.as_fd()), "drop did not restore");

        let bracket = AcceptBracket::set(copy.as_fd()).expect("set failed");
        bracket.restore().expect("restore failed");
        assert!(!nonblocking(socket.as_fd()), "restore did not restore");
    }

    /// The bracket restores the flags read at its own attempt, so a
    /// nonblocking mode set before it (by a sibling's `F_SETFL`) stands.
    #[test]
    fn accept_bracket_keeps_flags_read_at_its_attempt() {
        let socket = stream_socket();
        // SAFETY: fcntl on a descriptor this test owns.
        let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: as above.
        assert_eq!(
            unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        drop(AcceptBracket::set(socket.as_fd()).expect("set failed"));
        assert!(nonblocking(socket.as_fd()));
    }

    /// The modeled receive timeout of each value agrees with what the kernel
    /// stores for it, a negative one included (stored as zero, like none),
    /// and a model that missed a `setsockopt` does not.
    #[test]
    fn receive_timeout_model_agrees_with_the_kernel() {
        let socket = stream_socket();
        for (tv_sec, tv_usec) in [
            (1, 500_000),
            (0, 1),
            (3, 0),
            (-1, 0),
            (0, 0),
            (libc::time_t::MAX, 0),
        ] {
            let value = libc::timeval { tv_sec, tv_usec };
            // SAFETY: the pointer names a local of the size passed.
            let set = unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    (&raw const value).cast(),
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                )
            };
            assert_eq!(set, 0, "setsockopt failed: {}", io::Error::last_os_error());
            let kernel = kernel_receive_timeout(socket.as_fd()).expect("getsockopt failed");
            let modeled = ReceiveTimeout::set_by(value);
            assert!(
                receive_timeout_agrees(modeled, kernel),
                "{tv_sec}.{tv_usec:06}: modeled {modeled:?}, kernel {kernel:?}"
            );
        }
        assert_eq!(
            ReceiveTimeout::set_by(libc::timeval {
                tv_sec: -1,
                tv_usec: 0
            }),
            ReceiveTimeout::Immediate
        );
        assert_eq!(
            ReceiveTimeout::set_by(libc::timeval {
                tv_sec: libc::time_t::MAX,
                tv_usec: 0
            }),
            ReceiveTimeout::None
        );
        assert!(!receive_timeout_agrees(
            ReceiveTimeout::None,
            Duration::from_secs(1)
        ));
        assert!(!receive_timeout_agrees(
            ReceiveTimeout::After(Duration::from_secs(1)),
            Duration::ZERO
        ));
        assert!(!receive_timeout_agrees(
            ReceiveTimeout::Unknown,
            Duration::ZERO
        ));
    }

    /// Each accept removes only its own guard entry.
    #[test]
    fn accept_wait_removes_only_its_own_entry() {
        let table = Arc::new(Mutex::new(FileMetadata::new(
            detcore_model::pid::DetTid::from_raw(1),
        )));
        let first = AcceptWait::register(&table, 3);
        let second = AcceptWait::register(&table, 3);
        drop(first);
        assert_eq!(table.lock().unwrap().waited_listener_in(3..=3), Some(3));
        drop(second);
        assert_eq!(table.lock().unwrap().waited_listener_in(0..=u32::MAX), None);
    }
}
