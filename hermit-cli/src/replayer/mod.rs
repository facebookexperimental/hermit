/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[macro_use]
mod macros;

mod fs;
mod mmap;
mod network;
mod random;
mod time;

use std::path::PathBuf;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Rdtsc;
use reverie::RdtscResult;
use reverie::Subscription;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::Close;
use reverie::syscalls::EfdFlags;
use reverie::syscalls::Eventfd2;
use reverie::syscalls::FcntlCmd;
use reverie::syscalls::OFlag;
use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;
fn capture_guest_fd(pid: Pid, fd: libc::c_int) -> (Option<std::os::fd::OwnedFd>, Option<String>) {
    match crate::fd::duplicate_guest_fd(pid, fd) {
        Ok(duplicate) => (Some(duplicate), None),
        Err(error) if error.raw_os_error() == Some(libc::EBADF) => (None, None),
        Err(error) => (None, Some(error.to_string())),
    }
}

use crate::desync::DesyncError;
use crate::event_stream::DebugEvent;
use crate::event_stream::EventReader;
use crate::event_stream::normalize_unused_args;

/// A Reverie tool that replays syscalls. Note that only syscalls that cannot be
/// made deterministic are forwarded to this tool.
#[derive(Default, Serialize, Deserialize)]
pub struct Replayer {
    // Keep track of the data directory. Each thread uses this path to open its
    // event stream.
    data: PathBuf,
    /// Duplicates of this guest process's captured output endpoints.
    #[serde(skip)]
    stdout: Option<std::os::fd::OwnedFd>,
    #[serde(skip)]
    stderr: Option<std::os::fd::OwnedFd>,
    /// Preserve replayed write ordering independently for each captured stream.
    #[serde(skip)]
    stdout_output_lock: tokio::sync::Mutex<()>,
    #[serde(skip)]
    stderr_output_lock: tokio::sync::Mutex<()>,
    #[serde(skip)]
    stdout_error: Option<String>,
    #[serde(skip)]
    stderr_error: Option<String>,
}

#[reverie::tool]
impl Tool for Replayer {
    type GlobalState = detcore::GlobalState;
    type ThreadState = EventReader;

    fn new(pid: Pid, cfg: &<Self::GlobalState as GlobalTool>::Config) -> Self {
        let (stdout, stdout_error) = capture_guest_fd(pid, libc::STDOUT_FILENO);
        let (stderr, stderr_error) = capture_guest_fd(pid, libc::STDERR_FILENO);
        Self {
            data: cfg.replay_data.as_ref().unwrap().clone(),
            stdout,
            stderr,
            stdout_output_lock: tokio::sync::Mutex::new(()),
            stderr_output_lock: tokio::sync::Mutex::new(()),
            stdout_error,
            stderr_error,
        }
    }

    fn init_thread_state(
        &self,
        child: Tid,
        _parent: Option<(Tid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        // We have to unwrap because there is now way to handle errors here.
        EventReader::open(&self.data, child).unwrap_or_else(|err| {
            panic!(
                "Failed to open {:?} for thread {}: {}",
                self.data, child, err
            )
        })
    }

    fn subscriptions(config: &<Self::GlobalState as GlobalTool>::Config) -> Subscription {
        // Subscribe to the exact same events as the recorder does.
        crate::recorder::Recorder::subscriptions(config)
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        self.expect_syscall(guest, syscall);

        // NOTE: This match statement should be identical to the one in the
        // recorder. Otherwise, our recorder and replayer will disagree about
        // how syscalls should be handled.
        //
        // FIXME: Figure out a way to avoid duplicate code. (Merge record/replay
        // into a single tool?)
        Ok(match syscall {
            // We must let through execve without any modification. Recording
            // events for these is hard because execve only returns upon
            // failure.
            Syscall::Execve(_) => guest.inject(syscall).await,
            Syscall::Execveat(_) => guest.inject(syscall).await,
            Syscall::Brk(_) => self.let_through(guest, syscall).await,
            Syscall::Mprotect(_) => self.let_through(guest, syscall).await,
            Syscall::ArchPrctl(_) => {
                // To properly handle arch_prctl, we should prevent calls from
                // using ARCH_SET_CPUID since we already do that for the
                // tracees. However, it is rare for programs to use
                // ARCH_SET_CPUID. For all other arch_prctl subfunctions, we
                // should let it through.
                self.let_through(guest, syscall).await
            }
            Syscall::Read(syscall) => self.handle_read(guest, syscall).await,
            Syscall::Pread64(syscall) => self.handle_pread64(guest, syscall).await,
            Syscall::Readv(syscall) => {
                self.handle_readv_family(guest, syscall.iov().map(|a| a.as_raw()), syscall.len())
                    .await
            }
            Syscall::Preadv(syscall) => {
                self.handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len(),
                )
                .await
            }
            Syscall::Preadv2(syscall) => {
                self.handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len() as usize,
                )
                .await
            }
            Syscall::Recvfrom(syscall) => self.handle_recvfrom(guest, syscall).await,
            Syscall::Recvmsg(syscall) => self.handle_recvmsg(guest, syscall).await,
            Syscall::Write(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwrite64(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Writev(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwritev(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwritev2(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Access(_) => self.handle_simple(guest, syscall).await,
            Syscall::Lseek(_) => self.handle_simple(guest, syscall).await,
            Syscall::Stat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Fstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Lstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Newfstatat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Statfs(syscall) => self.handle_statfs(guest, syscall.buf()).await,
            Syscall::Fstatfs(syscall) => self.handle_statfs(guest, syscall.buf()).await,
            Syscall::Statx(syscall) => self.handle_statx(guest, syscall).await,
            Syscall::Getdents(syscall) => self.handle_getdents(guest, syscall).await,
            Syscall::Getdents64(syscall) => self.handle_getdents64(guest, syscall).await,
            Syscall::Mmap(syscall) => self.handle_mmap(guest, syscall).await,
            Syscall::Munmap(_) => self.let_through(guest, syscall).await,
            Syscall::Open(call) => {
                self.handle_virtual_fd_create(guest, call.flags().contains(OFlag::O_CLOEXEC))
                    .await
            }
            Syscall::Openat(call) => {
                self.handle_virtual_fd_create(guest, call.flags().contains(OFlag::O_CLOEXEC))
                    .await
            }
            Syscall::Close(_) => self.handle_close(guest, syscall).await,
            Syscall::Fchdir(_) => self.handle_simple(guest, syscall).await,
            Syscall::Fadvise64(_) => self.handle_simple(guest, syscall).await,
            Syscall::Flock(_) => self.handle_simple(guest, syscall).await,
            Syscall::Ftruncate(syscall) => self.handle_ftruncate(guest, syscall),
            Syscall::Dup(_) => self.handle_replayed_fd_operation(guest, syscall).await,
            Syscall::Dup2(_) => self.handle_dup2(guest, syscall).await,
            Syscall::Dup3(_) => self.handle_replayed_fd_operation(guest, syscall).await,
            Syscall::Ioctl(syscall) => self.handle_ioctl(guest, syscall).await,
            Syscall::Socket(_) => self.handle_replayed_fd_operation(guest, syscall).await,
            Syscall::ClockGettime(syscall) => self.handle_clock_gettime(guest, syscall).await,
            Syscall::Gettimeofday(syscall) => self.handle_gettimeofday(guest, syscall).await,
            Syscall::Settimeofday(_) => self.handle_simple(guest, syscall).await,
            Syscall::Time(syscall) => self.handle_time(guest, syscall).await,
            Syscall::Setsockopt(_) => {
                self.handle_replayed_side_effect(guest, syscall, "setsockopt")
                    .await
            }
            Syscall::Fcntl(call)
                if matches!(
                    call.cmd(),
                    FcntlCmd::F_DUPFD(_) | FcntlCmd::F_DUPFD_CLOEXEC(_)
                ) =>
            {
                self.handle_replayed_fd_operation(guest, syscall).await
            }
            Syscall::Fcntl(call) if matches!(call.cmd(), FcntlCmd::F_SETFD(_)) => {
                self.handle_replayed_fd_operation(guest, syscall).await
            }
            Syscall::Fcntl(_) => self.handle_simple(guest, syscall).await,
            Syscall::Connect(_) => self.handle_simple(guest, syscall).await,
            Syscall::Sendto(_) => self.handle_simple(guest, syscall).await,
            Syscall::Sendmsg(_) => self.handle_simple(guest, syscall).await,
            Syscall::Poll(syscall) => self.handle_poll(guest, syscall).await,
            Syscall::Ppoll(syscall) => self.handle_ppoll(guest, syscall).await,
            Syscall::EpollWait(syscall) => self.handle_epoll_wait(guest, syscall).await,
            Syscall::Getsockopt(syscall) => self.handle_sockopt_family(guest, syscall.into()).await,
            Syscall::Getpeername(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getsockname(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getrandom(syscall) => self.handle_getrandom(guest, syscall).await,
            Syscall::Readlink(syscall) => self.handle_readlink(guest, syscall).await,
            Syscall::Mkdir(_) => self.handle_simple(guest, syscall).await,
            Syscall::Unlink(_) => self.handle_simple(guest, syscall).await,
            Syscall::Unlinkat(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Other(Sysno::close_range, _) => self.handle_close_range(guest, syscall).await,
            unsupported => return Ok(guest.inject_with_retry(unsupported).await?),
        }?)
    }

    async fn handle_rdtsc_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        _request: Rdtsc,
    ) -> Result<RdtscResult, Errno> {
        next_event!(guest, Rdtsc)
    }
}

impl Replayer {
    pub(super) async fn reserve_replay_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        cloexec: bool,
    ) {
        let flags = if cloexec {
            EfdFlags::EFD_CLOEXEC
        } else {
            EfdFlags::empty()
        };
        let placeholder = guest
            .inject_with_retry(Eventfd2::new().with_count(0).with_flags(flags))
            .await
            .unwrap_or_else(|error| {
                panic!("could not reserve replay FD {fd} with an eventfd: {error}")
            });
        if placeholder != i64::from(fd) {
            let _ = guest.inject(Close::new().with_fd(placeholder as i32)).await;
            panic!(
                "replay FD namespace diverged: expected slot {fd}, placeholder returned {placeholder}"
            );
        }
    }

    async fn handle_virtual_fd_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        cloexec: bool,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if let Ok(fd) = recorded {
            self.reserve_replay_fd(guest, fd as i32, cloexec).await;
        }
        recorded
    }

    async fn handle_replayed_fd_operation<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if let Ok(expected) = recorded {
            let actual = guest
                .inject_with_retry(syscall)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "replayed FD operation {:?} failed after recording returned {expected}: {error}",
                        syscall
                    )
                });
            if actual != expected {
                panic!(
                    "replay FD namespace diverged for {:?}: recorded {expected}, replayed {actual}",
                    syscall
                );
            }
        }
        recorded
    }

    /// Replays the recorded result of `close` while preserving its physical FD
    /// namespace effect.
    async fn handle_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);

        // Linux releases a descriptor even when close reports EINTR, EIO,
        // ENOSPC, or EDQUOT. EBADF leaves the namespace unchanged, while
        // ERESTARTSYS means Reverie must restart the injection first.
        if !matches!(recorded, Err(Errno::EBADF | Errno::ERESTARTSYS))
            && let Err(error) = guest.inject(syscall).await
        {
            if error == Errno::EBADF {
                // Some replayed descriptor sources, notably SCM_RIGHTS,
                // currently have no physical peer.
                tracing::debug!(?syscall, "replayed close had no physical descriptor");
            } else {
                tracing::warn!(
                    ?error,
                    "physical close during replay differed from the recorded result"
                );
            }
        }

        recorded
    }

    // Check if we received the expected syscall or not.
    fn expect_syscall<G: Guest<Self>>(&self, guest: &mut G, syscall: Syscall) {
        let thread = guest.tid();
        let next_count = guest.thread_state().count + 1;
        let debug_event = guest
            .thread_state_mut()
            .next_debug_event()
            .unwrap_or_else(|source| {
                panic!(
                    "Replay syscall stream ended unexpectedly for recording {} on thread {} at event {} while the guest executed {:?}: {}",
                    self.data.display(),
                    thread,
                    next_count,
                    syscall,
                    source,
                )
            });

        // Compare only the argument registers the syscall actually uses. Reverie
        // keeps all six raw registers in every typed syscall and derives
        // `PartialEq` over them, so unused registers (which hold arbitrary
        // leftover guest values) would otherwise produce false desyncs for any
        // syscall with fewer than six arguments.
        if normalize_unused_args(debug_event.syscall()) == normalize_unused_args(syscall) {
            return;
        }

        if guest.is_root_thread() {
            // execve and execveat for the root thread are special cases. Even
            // when ASLR is turned off, these can have different pointer values
            // than what we originally recorded because the pointers originate
            // outside of the current address space.
            match syscall {
                Syscall::Execve(_) | Syscall::Execveat(_) => return,
                _ => {}
            }
        }

        let error = DesyncError {
            thread,
            count: guest.thread_state().count,
            actual: DebugEvent::new(syscall, &guest.memory()),
            expected: debug_event,
        };
        let summary = error.summary(&self.data, 16, 4).to_string();
        let report = match error.generate_report(&self.data) {
            Ok(report) => format!("Full desynchronization report: {}", report.display()),
            Err(report_error) => {
                format!("Could not write the full desynchronization report: {report_error}")
            }
        };

        panic!(
            "Replay diverged from recording {} on thread {} at syscall event {}. Re-record the workload with the same Hermit build after diagnosing the mismatch.\n{}\n{}",
            self.data.display(),
            thread,
            error.count,
            summary,
            report,
        );
    }

    /// Called for syscalls to explicitly let through. This should only be called
    /// for syscalls that cannot be recorded and are necessary for the program to
    /// function correctly. Examples of syscalls that fall into this category are
    /// ones that help with memory management (e.g., `brk`, `mprotect`, or
    /// `munmap`) or process management (e.g., `fork`, `vfork`, `clone`).
    ///
    /// For these syscalls, we just care about detecting dsynchronization and
    /// simply inject them to let them through.
    async fn let_through<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        // NOTE: Must use `inject_with_retry` here. Otherwise, we may end up
        // introducing non-determinism into the replay and popping multiple
        // syscall events.
        guest.inject_with_retry(syscall).await
    }

    /// Handles a syscall whose only value we care about is the return value
    /// (i.e., simple syscalls).
    ///
    /// For recording, this means we only record the return value of the syscall.
    /// For replay, this means we substitute the return value in lieu of actually
    /// performing the injection.
    ///
    /// The syscall must have two properties satisfied for this to be called:
    ///  1. The syscall must only have "input" arguments. That is, all arguments
    ///     must either be values or const pointers.
    ///  2. The execution of the program must not depend on anything else other
    ///     than the return value of the syscall. For example, `mmap` would violate
    ///     this rule since it affects later memory access.
    ///
    /// There are many syscalls who satisfy these two requirements.
    async fn handle_simple<G: Guest<Self>>(
        &self,
        guest: &mut G,
        _syscall: Syscall,
    ) -> Result<i64, Errno> {
        next_event!(guest, Return)
    }

    async fn handle_dup2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let actual = guest.inject_with_retry(syscall).await;
            // Some source descriptors are virtual: open-family syscalls replay
            // their recorded return value without creating a live kernel fd.
            // Preserve that behavior when there is nothing to duplicate in the
            // replay process.
            if actual != Err(Errno::EBADF) {
                assert_eq!(actual, recorded, "dup2 fd-table mutation diverged");
            }
        }
        recorded
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    async fn handle_replayed_side_effect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        operation: &str,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let actual = guest.inject_with_retry(syscall).await;
            assert_eq!(actual, recorded, "{operation} side effects diverged");
        }
        recorded
    }

    // TODO-HUMAN-REVIEW(#557): Audit close_range fd-table replay semantics.
    async fn handle_close_range<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Return);
        if recorded.is_ok() {
            let actual = guest.inject_with_retry(syscall).await;
            assert_eq!(actual, recorded, "close_range side effects diverged");
        }
        recorded
    }
}
