/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::num::NonZeroUsize;
use std::time::Duration;

use async_trait::async_trait;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::Displayable;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::ProtFlags;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Timespec;
use reverie::syscalls::WaitPidFlag;

use crate::fd::FdType;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::ExternalOpId;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::tool_global::ResumeStatus;
use crate::tool_global::resource_request;
use crate::tool_global::thread_observe_time;
use crate::tool_global::trace_schedevent;
use crate::tool_local::Detcore;
use crate::types::LogicalTime;
use crate::types::SchedEvent;
use crate::types::SyscallPhase;

impl<T: RecordOrReplay> Detcore<T> {
    /// Record or replay a BLOCKING syscall without stalling the current thread (and thus
    /// deadlocking).  This uses a protocol of an extra resource request before/after the
    /// syscall to inform the scheduler that the thread is leaving/rejoining the runnable
    /// threads pool.
    ///
    /// This is only valid to use (1) in hermit record/replay modes, or (2)
    /// when we're in "hermit run", but we're NOT sequentializing threads, because in
    /// that case it's ok to use the blocking versions of system calls.
    pub async fn record_or_replay_blocking<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let op_id = ExternalOpId::new(dettid, guest.thread_state().stats.syscall_count);
        // Internal-vs-external fd classification happens at the call sites that hold the
        // typed, nonblockize-able syscall (see execute_nonblockable_fd_syscall):
        // container-internal pipes are routed to the InternalIOPolling nonblockize-retry
        // path and must NOT reach this external-blocking protocol. BlockingExternalIO
        // deschedules the thread to run in the background and rejoin nondeterministically,
        // which is unsafe for a pipe whose reader and writer are interdependent -- doing
        // so is the root cause of the record/replay pipe deadlock. The remaining callers
        // (external poll, wait4) are external by construction (their fd is not a single
        // extractable internal pipe). Guard the invariant in debug builds while the
        // deterministic scheduler is active. With thread sequentialization disabled,
        // resource requests are no-ops and internal pipes intentionally use a blocking
        // host syscall, as documented by this method.
        debug_assert!(
            !self.cfg.sequentialize_threads || !syscall_targets_internal_fd(guest, call),
            "record_or_replay_blocking (BlockingExternalIO) reached for an internal pipe fd \
             on syscall {}; internal fds must use the InternalIOPolling path",
            call.name()
        );
        {
            let mut rsrcs = Resources::new(dettid);
            // With sequentialization enabled, only truly EXTERNAL endpoints reach here.
            // Without it, resource_request is a no-op and internal fds may block directly.
            rsrcs.insert(ResourceID::BlockingExternalIO(op_id), Permission::RW);
            rsrcs.fyi(call.name());
            resource_request(guest, rsrcs).await;
        }
        tracing::trace!(
            "Guest proceeding to execute potentially blocking call {}...",
            call.name()
        );
        let res = self.record_or_replay(guest, call).await;
        // N.B. BlockingExternalIO is a "oneshot" resource, so no need to release
        // explicitly here:
        {
            let mut rsrcs = Resources::new(dettid);
            rsrcs.insert(ResourceID::BlockedExternalContinue(op_id), Permission::RW);
            rsrcs.fyi(call.name());
            resource_request(guest, rsrcs).await;
        }
        Ok(res?)
    }

    /// Executes a nonblockable syscall according to the following strategy:
    /// - Record mode: Execute possibly blocking syscall
    /// - Run mode: Transform the syscall to nonblocking if required before executing
    ///
    /// These are fd-oriented syscalls in the sense that whether they block or not depends
    /// on whether NONBLOCK was set on the corresponding file descriptor.
    pub async fn execute_nonblockable_fd_syscall<
        G: Guest<Self>,
        C: SyscallInfo + NonblockableSyscall + Into<Syscall>,
    >(
        &self,
        guest: &mut G,
        call: C,
    ) -> Result<i64, Error> {
        let wrapped: Syscall = call.into();

        let action = ioaction_based_on_fd_status(guest, call);

        // Is this operation on a container-INTERNAL fd (currently: pipes)? Internal
        // pipes are made physically nonblocking even in record/replay (see
        // handle_pipe2), so they can take the deterministic InternalIOPolling
        // nonblockize-and-retry path. They must NOT be forced onto the
        // BlockingExternalIO path in R/R: a pipe reader and its paired writer are not
        // independent, so descheduling the reader as "external blocking IO" deadlocks
        // the sequentialized scheduler (the documented R/R pipe hang). Truly external
        // endpoints (host fds, network sockets) still use BlockingExternalIO. Sockets
        // are left external for now: there is no internal-vs-external socket detection
        // yet (see the handle_accept4 comment).
        let internal_fd = syscall_targets_internal_fd(guest, wrapped);

        if !self.cfg.sequentialize_threads
            || (self.cfg.recordreplay_modes && !internal_fd)
            || action == IOAction::Blocking
        {
            tracing::trace!(
                "NonblockableSyscall: executing in blocking mode after all: {}",
                call.name()
            );
            // We let these have nondeterminstic timing in record mode:
            Ok(self.record_or_replay_blocking(guest, wrapped).await?)
            // If in the future we want to record EXTERNAL network traffic only, we have a
            // challenge to overcome.  We don't know if we need to record until after the
            // accept completes, so we need an API for *post-facto* recording.
        } else if action == IOAction::NonblockizeRetry {
            tracing::trace!(
                "NonblockableSyscall: converting to nonblocking syscall (internal polling): {}",
                call.name()
            );
            let mut rsrc = Resources::new(guest.thread_state().dettid);
            rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
            rsrc.fyi(call.name());
            // In record/replay mode, route an internal-fd (pipe) read/write through the
            // record/replay subtool so its data is captured on record and reproduced on
            // replay (see retry_nonblocking_syscall). In plain `hermit run` there is no
            // recorder, so execute directly (subtool = None).
            let subtool = (self.cfg.recordreplay_modes && internal_fd).then_some(self);
            Ok(retry_nonblocking_syscall(guest, call, rsrc, subtool).await?)
        } else {
            assert!(action == IOAction::PassThru);
            tracing::trace!(
                "NonblockableSyscall: just passing it through: {}",
                call.name()
            );
            // Otherwise, the socket was already nonblocking, so we can safely execute it just once.
            Ok(self.record_or_replay(guest, wrapped).await?)
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#547)
    /// Complete a logically blocking pipe writev after Hermit has made the pipe physically
    /// nonblocking. A positive short write is an implementation artifact here: without
    /// O_NONBLOCK, Linux blocks until the full vector is written unless a signal or error
    /// interrupts it. Atomic vectors retain a private iovec snapshot for every retry; larger
    /// vectors advance a positive short-write remainder through scalar writes.
    pub async fn execute_blocking_pipe_writev<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Writev,
    ) -> Result<i64, Error> {
        const MAX_IOVECS: usize = 1024;
        // Linux limits a single vectored transfer to INT_MAX rounded down to a page.
        const MAX_RW_COUNT: usize = 0x7fff_f000;
        // Linux guarantees pipe writes through this size are atomic.
        const PIPE_BUF: usize = 4096;

        let Some(iov_addr) = call.iov() else {
            return self.execute_nonblockable_fd_syscall(guest, call).await;
        };
        if call.len() == 0 || call.len() > MAX_IOVECS {
            return self.execute_nonblockable_fd_syscall(guest, call).await;
        }

        let iovecs: Vec<(usize, usize)> = {
            let mut raw_iovecs = vec![
                libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                };
                call.len()
            ];
            guest.memory().read_values(iov_addr, &mut raw_iovecs)?;
            raw_iovecs
                .into_iter()
                .map(|iovec| (iovec.iov_base as usize, iovec.iov_len))
                .collect()
        };
        let requested = iovecs.iter().try_fold(0usize, |total, (_, length)| {
            total.checked_add(*length).ok_or(Errno::EINVAL)
        })?;
        if requested > isize::MAX as usize {
            return Err(Errno::EINVAL.into());
        }
        let target = requested.min(MAX_RW_COUNT);
        if target == 0 {
            return self.execute_nonblockable_fd_syscall(guest, call).await;
        }

        let atomic_pipe_write = target <= PIPE_BUF;

        tracing::trace!(
            "NonblockableSyscall: converting to nonblocking syscall (internal polling): writev"
        );
        let mut resources = Resources::new(guest.thread_state().dettid);
        resources.insert(ResourceID::InternalIOPolling, Permission::W);
        resources.fyi(call.name());
        let subtool = self.cfg.recordreplay_modes.then_some(self);
        let mut current = Syscall::Writev(call);
        let mut written_total = 0usize;

        loop {
            if resources.poll_attempt > 0
                && resource_request(guest, resources.clone()).await == ResumeStatus::Signaled
            {
                break if written_total > 0 {
                    Ok(written_total as i64)
                } else {
                    Err(call.signal_interrupt_errno().into())
                };
            }

            let result = if atomic_pipe_write {
                self.execute_atomic_pipe_writev_attempt(guest, call, &iovecs)
                    .await
            } else {
                match subtool {
                    Some(detcore) => detcore.record_or_replay(guest, current).await,
                    None => guest.inject_with_retry(current).await,
                }
            };
            match result {
                Ok(written) if written > 0 => {
                    let written = usize::try_from(written).map_err(|_| Errno::EIO)?;
                    written_total = written_total.checked_add(written).ok_or(Errno::EIO)?;
                    if written_total >= target {
                        break Ok(written_total as i64);
                    }
                    if atomic_pipe_write {
                        break Ok(written_total as i64);
                    }
                    current = match remaining_writev_segment(
                        call.fd(),
                        &iovecs,
                        written_total,
                        target - written_total,
                    ) {
                        Ok(Some(write)) => Syscall::Write(write),
                        Ok(None) => break Ok(written_total as i64),
                        Err(_) => break Ok(written_total as i64),
                    };
                }
                Ok(0) => break Ok(written_total as i64),
                Err(Errno::EAGAIN) => {
                    if !atomic_pipe_write && matches!(current, Syscall::Writev(_)) {
                        current = match remaining_writev_segment(call.fd(), &iovecs, 0, target) {
                            Ok(Some(write)) => Syscall::Write(write),
                            Ok(None) => break Ok(0),
                            Err(error) => break Err(error.into()),
                        };
                    }
                }
                Err(error) => {
                    break if written_total > 0 {
                        Ok(written_total as i64)
                    } else {
                        Err(error.into())
                    };
                }
                Ok(_) => break Err(Errno::EIO.into()),
            }

            resources.poll_attempt += 1;
            tracing::trace!(
                "Retry #{} for {}blocking pipe writev after {:?}: {}",
                resources.poll_attempt,
                if atomic_pipe_write { "atomic " } else { "" },
                result,
                call.display(&guest.memory())
            );
            record_retry_event(guest, call).await;
        }
    }

    async fn execute_atomic_pipe_writev_attempt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Writev,
        iovecs: &[(usize, usize)],
    ) -> Result<i64, Errno> {
        // Every backend provides at least 512 bytes of tool scratch. Linux's own fast-iovec
        // path is smaller; this covers common vectors without consuming guest VM mappings.
        const STACK_IOVECS: usize = 32;
        if iovecs.len() <= STACK_IOVECS {
            let mut stack = guest.stack().await;
            let scratch_array = {
                let mut raw_iovecs = [libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                }; STACK_IOVECS];
                for (raw, (base, length)) in raw_iovecs.iter_mut().zip(iovecs) {
                    raw.iov_base = *base as *mut libc::c_void;
                    raw.iov_len = *length;
                }
                stack.push(raw_iovecs)
            };
            let scratch_iov: Addr<libc::iovec> = scratch_array.cast();
            let _guard = stack
                .commit()
                .unwrap_or_else(|error| panic!("failed to commit atomic writev scratch: {error}"));
            let scratch_call = call.with_iov(Some(scratch_iov));
            return if self.cfg.recordreplay_modes {
                self.record_or_replay(guest, scratch_call).await
            } else {
                guest.inject_with_retry(scratch_call).await
            };
        }

        let mapping_len = iovecs
            .len()
            .checked_mul(std::mem::size_of::<libc::iovec>())
            .expect("validated iovec count cannot overflow scratch length");
        let mapped = guest
            .inject_with_retry(Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(mapping_len)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await
            .unwrap_or_else(|error| panic!("failed to map atomic writev scratch: {error}"));
        let mapped = usize::try_from(mapped)
            .unwrap_or_else(|_| panic!("atomic writev scratch mmap returned {mapped}"));
        let scratch_iov = Addr::<libc::iovec>::from_raw(mapped)
            .unwrap_or_else(|| panic!("atomic writev scratch mmap returned a null address"));
        let mapping_addr: Addr<libc::c_void> = scratch_iov.cast();
        let write_result = {
            let raw_iovecs: Vec<libc::iovec> = iovecs
                .iter()
                .map(|(base, length)| libc::iovec {
                    iov_base: *base as *mut libc::c_void,
                    iov_len: *length,
                })
                .collect();
            // SAFETY: the injected anonymous mapping is exclusively owned scratch space.
            guest
                .memory()
                .write_values(unsafe { scratch_iov.into_mut() }, &raw_iovecs)
        };
        if let Err(write_error) = write_result {
            guest
                .inject_with_retry(Syscall::Munmap(
                    syscalls::Munmap::new()
                        .with_addr(Some(mapping_addr))
                        .with_len(mapping_len),
                ))
                .await
                .unwrap_or_else(|cleanup_error| {
                    panic!(
                        "failed to populate atomic writev scratch ({write_error}); cleanup failed ({cleanup_error})"
                    )
                });
            panic!("failed to populate atomic writev scratch: {write_error}");
        }

        let scratch_call = call.with_iov(Some(scratch_iov));
        let result = if self.cfg.recordreplay_modes {
            self.record_or_replay(guest, scratch_call).await
        } else {
            guest.inject_with_retry(scratch_call).await
        };
        guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Some(mapping_addr))
                    .with_len(mapping_len),
            ))
            .await
            .unwrap_or_else(|error| panic!("failed to unmap atomic writev scratch: {error}"));
        result
    }

    /// Override physically_nonblocking to true for the file descriptor, if appropriate.
    pub fn maybe_set_nonblocking_fd<G: Guest<Self>>(&self, guest: &G, fd: i32) {
        if self.cfg.sequentialize_threads && !self.cfg.debug_externalize_sockets {
            guest
                .thread_state()
                .with_detfd(fd, |detfd| {
                    detfd.set_physically_nonblocking();
                })
                .unwrap();
        }
    }
}

fn remaining_writev_segment(
    fd: i32,
    iovecs: &[(usize, usize)],
    mut consumed: usize,
    remaining_limit: usize,
) -> Result<Option<syscalls::Write>, Errno> {
    for (base, length) in iovecs {
        if consumed >= *length {
            consumed -= *length;
            continue;
        }
        let base = base.checked_add(consumed).ok_or(Errno::EFAULT)?;
        let buffer = Addr::<u8>::from_raw(base).ok_or(Errno::EFAULT)?;
        return Ok(Some(
            syscalls::Write::new()
                .with_fd(fd)
                .with_buf(Some(buffer))
                .with_len((*length - consumed).min(remaining_limit)),
        ));
    }
    Ok(None)
}

/// A blocking syscall that involves a fail descriptor may be handled in these three ways:
#[derive(PartialEq, Eq, Debug)]
pub enum IOAction {
    /// It may physically block and we can't change that.  Treat it as ExternalBlockingIO.
    Blocking,
    /// We can nonblockize and retry the call.
    NonblockizeRetry,
    /// The call is nonblocking already, and safe to execute.
    PassThru,
}

/// Returns strategy based on FD-based call may actually block when executed.
pub fn ioaction_based_on_fd_status<
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
    C: SyscallInfo + Into<Syscall>,
>(
    guest: &mut G,
    call: C,
) -> IOAction {
    let wrapped: Syscall = call.into();
    let fd = get_fd(wrapped).unwrap_or_else(|| panic!("Failed to get fd for {}", call.name()));
    let (phys, virt) = guest
        .thread_state()
        .with_detfd(fd, |detfd| {
            (detfd.physically_nonblocking(), detfd.is_nonblocking())
        })
        .unwrap();
    tracing::trace!(
        "Checking FD {} for nonblocking: physical {} / virtual {}",
        fd,
        phys,
        virt
    );
    if virt && !phys {
        // TF: simulate nonblocking on top of physically blocking? How?
        panic!(
            "Invariant violation, fd {}: we cannot simulate nonblocking behavior when set to blocking mode in the kernel.",
            fd
        );
    } else if !virt && !phys {
        // FF: logically blocking, physically blocking, this could only work with BlockingExternalIO.
        IOAction::Blocking
    } else if virt && phys {
        // TT: both nonblocking, so firing once is sufficient
        IOAction::PassThru
    } else {
        // FT: Need to simulate blocking on top of nonblocking.
        IOAction::NonblockizeRetry
    }
}

/// Does this single-fd syscall operate on a container-INTERNAL file descriptor?
///
/// Currently this recognizes pipes, whose two endpoints are always both owned by guest
/// processes inside the deterministic container. Internal pipes are made physically
/// nonblocking (see `handle_pipe2`) so a potentially-blocking op on them can use the
/// deterministic `InternalIOPolling` nonblockize-and-retry strategy instead of
/// `BlockingExternalIO`. Treating an internal pipe as external blocking IO deadlocks
/// the sequentialized scheduler in record/replay, because a pipe reader and its paired
/// writer are not independent.
///
/// Sockets are intentionally NOT classified as internal here: there is no reliable
/// internal-vs-external socket detection yet (loopback / AF_UNIX-to-another-guest vs a
/// real host peer), so sockets conservatively remain external. Syscalls whose fd is not
/// directly extractable (e.g. poll/ppoll, which carry a pointer to an fd array) return
/// false and keep their existing handling.
pub fn syscall_targets_internal_fd<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &mut G,
    call: Syscall,
) -> bool {
    match get_fd(call) {
        Some(fd) => guest
            .thread_state()
            .with_detfd(fd, |detfd| matches!(detfd.ty(), FdType::Pipe))
            .unwrap_or(false),
        None => false,
    }
}

/// A large subset of system calls have a single, unique file descriptor argument.  This
/// is a convenience function for grabbing that argument.
///
/// It does not cover system calls with multiple fd arguments, with pointers to heap
/// structures that contain fds.
fn get_fd(s: Syscall) -> Option<i32> {
    match s {
        Syscall::Recvfrom(s) => Some(s.fd()),
        Syscall::Recvmsg(s) => Some(s.sockfd()),
        Syscall::Recvmmsg(s) => Some(s.fd()),
        Syscall::Sendto(s) => Some(s.fd()),
        Syscall::Sendmsg(s) => Some(s.fd()),
        Syscall::Sendmmsg(s) => Some(s.sockfd()),
        Syscall::Accept(s) => Some(s.sockfd()),
        Syscall::Accept4(s) => Some(s.sockfd()),
        Syscall::Connect(s) => Some(s.fd()),
        Syscall::Bind(s) => Some(s.fd()),
        Syscall::Listen(s) => Some(s.fd()),
        Syscall::Getsockname(s) => Some(s.fd()),
        Syscall::Getpeername(s) => Some(s.fd()),
        Syscall::Setsockopt(s) => Some(s.fd()),
        Syscall::Getsockopt(s) => Some(s.fd()),

        Syscall::Read(s) => Some(s.fd()),
        Syscall::Write(s) => Some(s.fd()),
        Syscall::Close(s) => Some(s.fd()),
        Syscall::Fstat(s) => Some(s.fd()),
        Syscall::Lseek(s) => Some(s.fd()),
        Syscall::Mmap(s) => Some(s.fd()),
        Syscall::Ioctl(s) => Some(s.fd()),
        Syscall::Pread64(s) => Some(s.fd()),
        Syscall::Pwrite64(s) => Some(s.fd()),
        Syscall::Readv(s) => Some(s.fd()),
        Syscall::Writev(s) => Some(s.fd()),

        Syscall::Shutdown(s) => Some(s.fd()),
        Syscall::Fcntl(s) => Some(s.fd()),
        Syscall::Flock(s) => Some(s.fd()),
        Syscall::Fsync(s) => Some(s.fd()),
        Syscall::Fdatasync(s) => Some(s.fd()),
        Syscall::Ftruncate(s) => Some(s.fd()),
        Syscall::Fchdir(s) => Some(s.fd()),
        Syscall::Fchmod(s) => Some(s.fd()),
        Syscall::Fchown(s) => Some(s.fd()),
        Syscall::Fstatfs(s) => Some(s.fd()),
        Syscall::Readahead(s) => Some(s.fd()),
        Syscall::Fsetxattr(s) => Some(s.fd()),
        Syscall::Fgetxattr(s) => Some(s.fd()),
        Syscall::Flistxattr(s) => Some(s.fd()),
        Syscall::Fremovexattr(s) => Some(s.fd()),
        Syscall::Fadvise64(s) => Some(s.fd()),
        Syscall::InotifyAddWatch(s) => Some(s.fd()),
        Syscall::InotifyRmWatch(s) => Some(s.fd()),
        Syscall::SyncFileRange(s) => Some(s.fd()),
        Syscall::Vmsplice(s) => Some(s.fd()),
        Syscall::Utimensat(s) => Some(s.dirfd()),
        Syscall::Signalfd(s) => Some(s.fd()),
        Syscall::Fallocate(s) => Some(s.fd()),
        Syscall::TimerfdSettime(s) => Some(s.fd()),
        Syscall::TimerfdGettime(s) => Some(s.fd()),
        Syscall::Signalfd4(s) => Some(s.fd()),
        Syscall::Preadv(s) => Some(s.fd()),
        Syscall::Pwritev(s) => Some(s.fd()),
        Syscall::Syncfs(s) => Some(s.fd()),
        Syscall::Setns(s) => Some(s.fd()),
        Syscall::FinitModule(s) => Some(s.fd()),
        Syscall::Preadv2(s) => Some(s.fd()),
        Syscall::Pwritev2(s) => Some(s.fd()),

        Syscall::Openat(s) => Some(s.dirfd()),
        Syscall::Mkdirat(s) => Some(s.dirfd()),
        Syscall::Mknodat(s) => Some(s.dirfd()),
        Syscall::Fchownat(s) => Some(s.dirfd()),
        Syscall::Futimesat(s) => Some(s.dirfd()),
        Syscall::Newfstatat(s) => Some(s.dirfd()),
        Syscall::Unlinkat(s) => Some(s.dirfd()),
        Syscall::Readlinkat(s) => Some(s.dirfd()),
        Syscall::Fchmodat(s) => Some(s.dirfd()),
        Syscall::Faccessat(s) => Some(s.dirfd()),
        Syscall::NameToHandleAt(s) => Some(s.dirfd()),
        Syscall::Execveat(s) => Some(s.dirfd()),
        Syscall::Statx(s) => Some(s.dirfd()),
        Syscall::Symlinkat(s) => Some(s.newdirfd()),
        Syscall::PerfEventOpen(s) => Some(s.group_fd()),
        Syscall::OpenByHandleAt(s) => Some(s.mount_fd()),

        Syscall::EpollCtl(s) => Some(s.epfd()),
        Syscall::EpollWait(s) => Some(s.epfd()),
        Syscall::EpollPwait(s) => Some(s.epfd()),

        // Ambiguous, 2 fds, no answer:
        Syscall::Dup2(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::Sendfile(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::Renameat(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::Linkat(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::FanotifyMark(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::Renameat2(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::Dup3(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::KexecLoad(_) => None,

        // Takes a pointer to fd, not directly accessible:
        Syscall::Poll(_) => None,
        // Takes a pointer to fd, not directly accessible:
        Syscall::Ppoll(_) => None,

        _ => None,
    }
}

/// A system call which may or may not block, but which can be MADE nonblocking.
#[async_trait]
pub trait NonblockableSyscall: SyscallInfo {
    /// Convert the system call to a nonblocking version of itself.  Sometimes this means
    /// setting a zero timeout, and sometimes it means something else.
    ///
    /// This may need to stack allocate, so it returns a StackGuard.
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>);

    /// Check if the result (in nonblocking mode) is analogous to blocking in blocking mode.
    /// I.e. the result means "try again".
    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Ok(0)
    }

    /// Return the errno used when a signal interrupts this internally polled syscall.
    /// Most blocking I/O is restartable when its handler uses `SA_RESTART`.
    fn signal_interrupt_errno(&self) -> Errno {
        Errno::ERESTARTSYS
    }

    /// Convert a physical nonblocking completion into the result expected by the guest.
    /// `retried` is true after a prior result was classified as blocked.
    fn normalize_nonblocking_result(
        &self,
        res: Result<i64, Errno>,
        _retried: bool,
    ) -> Result<i64, Errno> {
        res
    }
}

/// A system call which can logically timeout and then would return a given value
/// indicating that timeout.
pub trait TimeoutableSyscall: SyscallInfo {
    /// What would the syscall return IF it timed out?
    fn timeout_return_val(&self) -> Result<i64, Errno>;
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Poll {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        _guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        (self.with_timeout(0), None)
    }

    fn signal_interrupt_errno(&self) -> Errno {
        Errno::EINTR
    }
}

impl TimeoutableSyscall for reverie::syscalls::Poll {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Ok(0)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Ppoll {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        let (tp, guard) = zero_timespec(guest).await;
        // SAFETY: `tp` points to exclusively owned scratch storage kept alive by `guard`.
        let tp = unsafe { tp.into_mut() };
        (self.with_timeout(Some(tp)), Some(guard))
    }

    fn signal_interrupt_errno(&self) -> Errno {
        Errno::EINTR
    }
}

impl TimeoutableSyscall for reverie::syscalls::Ppoll {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Ok(0)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::EpollWait {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        _guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        (self.with_timeout(0), None)
    }

    fn signal_interrupt_errno(&self) -> Errno {
        Errno::EINTR
    }
}

impl TimeoutableSyscall for reverie::syscalls::EpollWait {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Ok(0)
    }
}

async fn zero_timespec<'stack, T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &mut G,
) -> (Addr<'stack, Timespec>, <G::Stack as Stack>::StackGuard) {
    let mut stack = guest.stack().await;
    let tp_val = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let tp = stack.push(tp_val);
    let guard = stack.commit().expect("stack.commit to succeed");
    (tp, guard)
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Wait4 {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        _guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        let call2 = self.with_options(self.options() | WaitPidFlag::WNOHANG);
        (call2, None)
    }

    // Child has not changed state yet, so we go to the scheduler and wait to poll again.
    // In scenarios with lots of outstanding waits, this polling strategy can change the asymptotic
    // complexity of the program. Ideally, we would model the blocking `wait4` (and process state
    // transitions) directly in the scheduler, and execute it only when we know it will complete.
    //
    // The polling backoff strategy mitigates this problem however.
    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Ok(0)
    }
}

#[async_trait]
/// Used only for FUTEX_WAIT
impl NonblockableSyscall for reverie::syscalls::Futex {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        let (tp, guard) = zero_timespec(guest).await;
        (self.with_timeout(Some(tp)), Some(guard))
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        // EAGAIN can mean the futex wait's compare-and-block failed and we should return that to
        // the guest.  With timeout=0, the timeout is what shows that it would have blocked.
        res == Err(Errno::ETIMEDOUT)
    }
}

impl TimeoutableSyscall for reverie::syscalls::Futex {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Err(Errno::ETIMEDOUT)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::RtSigtimedwait {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        // This is a bit more complicated because we need a new timespec to point to in
        // the guest memory.
        let (tp, guard) = zero_timespec(guest).await;
        (self.with_timeout(Some(tp)), Some(guard))
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN)
    }

    fn signal_interrupt_errno(&self) -> Errno {
        Errno::EINTR
    }
}

impl TimeoutableSyscall for reverie::syscalls::RtSigtimedwait {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Err(Errno::EAGAIN)
    }
}

/// While the read syscall is quite general, this nonblocking capacity is used
/// ONLY for sockets and pipes.
#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Read {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        // A return value of Ok(0) indicates end of file.
        // Note that we've ruled out 0-count reads before this point.
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

/// While the read syscall is quite general, this nonblocking capacity is used
/// ONLY for sockets and pipes.
#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Write {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        // A return value of Ok(0) indicates end of file.
        // Note that we've ruled out 0-count reads before this point.
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#547)
/// Vectored writes have the same blocking behavior as scalar writes on pipes and sockets.
#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Writev {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

/// A common helper shared among several network syscalls.
/// We can't actually CONVERT these syscalls into nonblocking, but we can assert that they are by
/// checking the status of their file descriptor.
fn network_comm_syscall<T: RecordOrReplay, G: Guest<Detcore<T>>, C: SyscallInfo + Into<Syscall>>(
    call: C,
    guest: &mut G,
) -> (C, Option<<G::Stack as Stack>::StackGuard>) {
    // Already nonblocking because we've assured the socket is.
    let fd = get_fd(call.into()).unwrap_or_else(|| {
        panic!(
            "network_comm_syscall called on invalid syscall / unknown fd: {}",
            call.name()
        );
    });
    guest
        .thread_state()
        .with_detfd(fd, |detfd| {
            assert!(
                detfd.physically_nonblocking(),
                "expecting sockets/pipes to be physically nonblocking"
            );
        })
        .unwrap();
    (call, None)
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Accept4 {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

impl TimeoutableSyscall for reverie::syscalls::Accept4 {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Ok(0)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Recvfrom {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Recvmsg {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Recvmmsg {
    // This system call has a timeout argument, but we ignore it because the underlying
    // socket is nonblocking anyway (in runs where we call this).
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Sendto {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Sendmmsg {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Sendmsg {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Connect {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN)
            || res == Err(Errno::EWOULDBLOCK)
            || res == Err(Errno::EINPROGRESS)
            || res == Err(Errno::EALREADY)
    }

    fn normalize_nonblocking_result(
        &self,
        res: Result<i64, Errno>,
        retried: bool,
    ) -> Result<i64, Errno> {
        match (retried, res) {
            (true, Err(Errno::EISCONN)) => Ok(0),
            (_, res) => res,
        }
    }
}

/// Transform a syscall to nonblocking, then retry it until it returns a successful result.
/// Retry a nonblockizable syscall (e.g. a pipe/socket read or write) until it succeeds.
///
/// `subtool` selects how each poll iteration executes the underlying syscall. Pass
/// `Some(detcore)` in record/replay mode for a container-INTERNAL fd (currently pipes):
/// each iteration is then routed through `Detcore::record_or_replay`, so the read's
/// bytes (and every intervening `EAGAIN`) are captured in the recording and reproduced
/// verbatim on replay. Without this, an internal-pipe read on the InternalIOPolling path
/// bypasses the recorder and replay reads live from a pipe whose cross-process writer
/// schedule is not reproduced -- the reader sees EOF instead of the recorded data and
/// replay desyncs. Pass `None` for plain `hermit run` (no recording) or for external fds.
pub async fn retry_nonblocking_syscall<T, G, C>(
    guest: &mut G,
    call: C,
    rsrc: Resources,
    subtool: Option<&Detcore<T>>,
) -> Result<i64, Error>
where
    C: NonblockableSyscall + Into<Syscall>,
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    // Bogus 99 return value is dead code below:
    retry_nonblocking_syscall_helper(guest, call, rsrc, None, subtool).await
}

/// Retry a non-blocking syscall until it succeeds. Set the timeout to zero for the actual
/// syscalls (retries), while monitoring the clock to see if/when the logical timeout
/// should trigger.  Timeout is passed as an ABSOLUTE TIME (not duration).
pub async fn retry_nonblocking_syscall_with_timeout<T, G, C>(
    guest: &mut G,
    call: C,
    rsrc: Resources,
    // Logical timeout:
    maybe_timeout: Option<LogicalTime>,
) -> Result<i64, Error>
where
    C: NonblockableSyscall + TimeoutableSyscall + Into<Syscall>,
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let maybe_tup = maybe_timeout.map(|t| (t, call.timeout_return_val()));
    // poll/epoll_wait/futex/rt_sigtimedwait keep their existing execution (raw
    // inject_with_retry): their record/replay handling is out of scope for the internal
    // pipe data-ordering fix, and their fds are not necessarily internal pipes.
    retry_nonblocking_syscall_helper(guest, call, rsrc, maybe_tup, None).await
}

// Private helper.
async fn retry_nonblocking_syscall_helper<T, G, C>(
    guest: &mut G,
    call0: C,
    rsrc: Resources,
    maybe_timeout: Option<(LogicalTime, Result<i64, Errno>)>,
    subtool: Option<&Detcore<T>>,
) -> Result<i64, Error>
where
    C: NonblockableSyscall + Into<Syscall>,
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    // The stack-allocated memory here needs to live across the loop, which means
    // surviving multiple syscall injections:
    let (call, _maybe_stackguard) = call0.into_nonblocking(guest).await;
    let mut rsrc = rsrc.clone();

    loop {
        if resource_request(guest, rsrc.clone()).await == ResumeStatus::Signaled {
            let errno = call.signal_interrupt_errno();
            tracing::trace!(
                "retry_nonblocking_syscall: interrupted by signal before retrying {}: {:?}",
                call.display(&guest.memory()),
                errno
            );
            return Err(errno.into());
        }
        // Route through the record/replay subtool for internal pipes so each poll (an
        // EAGAIN, or the final data-bearing read) becomes one recorded event that replay
        // reproduces deterministically; otherwise execute the syscall directly.
        let res = match subtool {
            Some(detcore) => detcore.record_or_replay(guest, call).await,
            None => guest.inject_with_retry(call).await,
        };
        if call.syscall_would_have_blocked(res) {
            rsrc.poll_attempt += 1;
            if let Some((timeout, timeout_result)) = maybe_timeout {
                let new_time = thread_observe_time(guest).await;
                if new_time >= timeout {
                    tracing::trace!(
                        "Timing out syscall after #{} retries: {}",
                        rsrc.poll_attempt - 1,
                        call.display(&guest.memory())
                    );
                    return timeout_result.map_err(|e| e.into());
                } else {
                    tracing::trace!(
                        "Retry #{} for syscall due to result {:?}, {} from timeout: {}",
                        rsrc.poll_attempt,
                        res,
                        timeout - new_time,
                        call.display(&guest.memory())
                    );
                    record_retry_event(guest, call).await;
                }
            } else {
                tracing::trace!(
                    "Retry #{} for syscall due to result {:?}: {}",
                    rsrc.poll_attempt,
                    res,
                    call.display(&guest.memory())
                );
                record_retry_event(guest, call).await;
            }
        } else {
            let res = call
                .normalize_nonblocking_result(res, rsrc.poll_attempt > 0)
                .map_err(|e| e.into());
            tracing::trace!(
                "retry_nonblocking_syscall: syscall completed after {} retries: {} = {:?}",
                rsrc.poll_attempt,
                call.display(&guest.memory()),
                res
            );
            return res;
        }
    }
}

pub(crate) async fn record_retry_event<G, C, T>(guest: &mut G, call: C)
where
    C: SyscallInfo,
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let dettid = guest.thread_state().dettid;
    let cfg = &guest.config();
    if cfg.sequentialize_threads && cfg.should_trace_schedevent() {
        trace_schedevent(
            guest,
            with_guest_time(
                guest,
                SchedEvent::syscall(dettid, call.number(), SyscallPhase::Polling),
            ),
            true,
        )
        .await;
    }
}

// A helper function for enriching the schedevent with local information.
pub fn with_guest_time<G, T>(guest: &G, event: SchedEvent) -> SchedEvent
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettime = &guest.thread_state().thread_logical_time;
    event.with_dettime(dettime)
}

// Enrich the event with the RIP register from the current guest state, but only if it is unset.
pub async fn with_guest_rip<G, T>(guest: &mut G, mut event: SchedEvent) -> SchedEvent
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    assert!(event.end_rip.is_none());

    let regs = guest.regs().await;
    let end_rip = NonZeroUsize::new(regs.rip.try_into().unwrap()).unwrap();
    event.end_rip = Some(end_rip);
    event
}

// Convert to absolute logical time point for the timeout.
// 0 duration means no timeout, and this will return None.
pub async fn millis_duration_to_absolute_timeout<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &mut G,
    timeout_millis: i32,
) -> Option<LogicalTime> {
    if timeout_millis > 0 {
        nanos_duration_to_absolute_timeout(guest, (timeout_millis as u128) * 1000).await
    } else {
        None
    }
}

// Convert to absolute logical time point for the timeout.
// 0 duration means no timeout, and this will return None.
pub async fn nanos_duration_to_absolute_timeout<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &mut G,
    timeout_nanos: u128,
) -> Option<LogicalTime> {
    if timeout_nanos > 0 {
        let ns_delta = Duration::from_nanos(timeout_nanos as u64);
        let base_time = thread_observe_time(guest).await;
        let target_time = base_time + ns_delta;
        Some(target_time)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_nonblocking_results() {
        let call = reverie::syscalls::Connect::new();
        assert!(call.syscall_would_have_blocked(Err(Errno::EINPROGRESS)));
        assert!(call.syscall_would_have_blocked(Err(Errno::EALREADY)));
        assert_eq!(
            call.normalize_nonblocking_result(Err(Errno::EISCONN), true),
            Ok(0)
        );
        assert_eq!(
            call.normalize_nonblocking_result(Err(Errno::EISCONN), false),
            Err(Errno::EISCONN)
        );
    }

    #[test]
    fn signal_interruption_errno_matches_linux_restart_policy() {
        assert_eq!(
            reverie::syscalls::Poll::new().signal_interrupt_errno(),
            Errno::EINTR
        );
        assert_eq!(
            reverie::syscalls::Ppoll::new().signal_interrupt_errno(),
            Errno::EINTR
        );
        assert_eq!(
            reverie::syscalls::EpollWait::new().signal_interrupt_errno(),
            Errno::EINTR
        );
        let sigtimedwait = reverie::syscalls::RtSigtimedwait::new();
        assert_eq!(sigtimedwait.signal_interrupt_errno(), Errno::EINTR);
        assert!(sigtimedwait.syscall_would_have_blocked(Err(Errno::EAGAIN)));
        assert_eq!(sigtimedwait.timeout_return_val(), Err(Errno::EAGAIN));
        assert_eq!(
            reverie::syscalls::Read::new().signal_interrupt_errno(),
            Errno::ERESTARTSYS
        );
        assert_eq!(
            reverie::syscalls::Futex::new().signal_interrupt_errno(),
            Errno::ERESTARTSYS
        );
    }
}
