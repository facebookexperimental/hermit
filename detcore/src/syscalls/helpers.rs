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
use reverie::syscalls::AddrMut;
use reverie::syscalls::Displayable;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::ProtFlags;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie::syscalls::Timespec;
use reverie::syscalls::WaitPidFlag;

use crate::fd::FdType;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::ExternalOpId;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::threads::BlockedWaitSignalError;
use crate::syscalls::threads::KERNEL_SIGSET_SIZE;
use crate::syscalls::threads::KernelSigaction;
use crate::syscalls::threads::KernelSignalState;
use crate::syscalls::threads::KernelSigset;
use crate::syscalls::threads::SignalQueue;
use crate::syscalls::threads::WaitSignalDisposition;
use crate::syscalls::threads::block_signals_for_disposition;
use crate::syscalls::threads::blocked_signal_mask;
use crate::syscalls::threads::kernel_sigset_bit;
use crate::syscalls::threads::read_thread_wait_signal_state;
use crate::syscalls::threads::read_wait_signal_state;
use crate::syscalls::threads::restore_signals_after_disposition;
use crate::syscalls::threads::wait_signal_disposition;
use crate::tool_global::ResumeStatus;
use crate::tool_global::host_timed_signals;
use crate::tool_global::resource_request;
use crate::tool_global::thread_observe_time;
use crate::tool_global::trace_schedevent;
use crate::tool_local::Detcore;
use crate::tool_local::finish_partial_record_or_replay_write;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::OpenFileId;
use crate::types::SchedEvent;
use crate::types::SyscallPhase;

impl<T: RecordOrReplay> Detcore<T> {
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// Apply the established unsupported-syscall refusal policy to a *supported*
    /// syscall whose particular operation Detcore cannot serve deterministically.
    ///
    /// This exists so such a refusal cannot invent its own policy. Three separate
    /// config knobs govern how a fail-closed run dies -- `shutdown_on_unsupported_syscall`
    /// (hard `exit(1)` through `unrecoverable_shutdown`), `exit_on_unsupported_syscall`
    /// (a typed `UnsupportedSyscallError` the backend terminates on without
    /// unwinding), and the `panic!` fallback -- and `Detcore::handle_unsupported_syscall`
    /// consults all three in that order. The normal `hermit run` CLI happens to set
    /// `shutdown_on_unsupported_syscall = panic_on_unsupported_syscalls`, so reading
    /// only the latter looks equivalent, but that coupling is a CLI default, not an
    /// invariant: an embedder that sets just `exit_on_unsupported_syscall` would get a
    /// process-wide `exit(1)` from a bespoke call site where the standard path returns
    /// a catchable error.
    ///
    /// When the run is *not* fail-closed, the caller's `fallback` errno is returned,
    /// because the operation itself is legal and the guest is entitled to a normal
    /// failure code (`handle_unsupported_syscall` passes the call through instead,
    /// which is not available here -- passing through is precisely the thing the
    /// caller has determined it cannot do).
    pub(crate) async fn refuse_unserviceable_operation<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: reverie::syscalls::Sysno,
        fallback: Errno,
    ) -> Result<i64, Error> {
        refuse_unserviceable_operation_with(
            guest,
            self.cfg.panic_on_unsupported_syscalls,
            sysno,
            fallback,
        )
        .await
    }

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
        self.record_or_replay_blocking_with_mask(guest, call, None)
            .await
    }

    /// `record_or_replay_blocking` for a call that sleeps under its own
    /// temporary signal mask (`ppoll`, `pselect6`, or an `rt_sigsuspend` that
    /// finds a signal already pending): `blocked_signal_mask` is the mask the
    /// kernel installs for the call, read in this thread's turn
    /// (`Resources::blocked_signal_mask`). `None` means the call sleeps under
    /// the thread's own mask.
    pub async fn record_or_replay_blocking_with_mask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        blocked_signal_mask: Option<u64>,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let op_id = ExternalOpId::new(dettid, guest.thread_state().stats.syscall_count);
        self.record_or_replay_blocking_resource(
            guest,
            call,
            ResourceID::BlockingExternalIO(op_id),
            blocked_signal_mask,
            false,
        )
        .await
    }

    /// `record_or_replay_blocking` for a call that sleeps under a temporary
    /// mask its turn cannot vouch for (`Resources::blocked_signal_mask_unknown`):
    /// the scheduler records no sleeping mask for it.
    pub async fn record_or_replay_blocking_with_unknown_mask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        self.record_or_replay_blocking_with_unknown_mask_then(guest, call, |_| {})
            .await
    }

    /// `record_or_replay_blocking_with_unknown_mask`, running `after_grant` once
    /// the scheduler has granted the call and before it runs: after any
    /// sibling that ran while the request waited. In replay nothing else runs
    /// until the served call finishes; in recording a sibling turn may still
    /// run before the call enters the kernel.
    pub async fn record_or_replay_blocking_with_unknown_mask_then<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        after_grant: impl FnOnce(&mut G),
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let op_id = ExternalOpId::new(dettid, guest.thread_state().stats.syscall_count);
        self.record_or_replay_blocking_resource_then(
            guest,
            call,
            ResourceID::BlockingExternalIO(op_id),
            None,
            true,
            after_grant,
        )
        .await
    }

    /// Execute the real `rt_sigsuspend` outside the runnable set while preserving
    /// its signal-only completion condition for the scheduler. `temporary_mask`
    /// is the mask the call sleeps under, as `Resources::blocked_signal_mask`
    /// describes.
    pub async fn record_or_replay_rt_sigsuspend<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigsuspend,
        temporary_mask: u64,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let op_id = ExternalOpId::new(dettid, guest.thread_state().stats.syscall_count);
        self.record_or_replay_blocking_resource(
            guest,
            call.into(),
            ResourceID::BlockingRtSigsuspend(op_id),
            Some(temporary_mask),
            false,
        )
        .await
    }

    async fn record_or_replay_blocking_resource<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        blocking_resource: ResourceID,
        blocked_signal_mask: Option<u64>,
        blocked_signal_mask_unknown: bool,
    ) -> Result<i64, Error> {
        self.record_or_replay_blocking_resource_then(
            guest,
            call,
            blocking_resource,
            blocked_signal_mask,
            blocked_signal_mask_unknown,
            |_| {},
        )
        .await
    }

    /// `record_or_replay_blocking_resource`, running `after_grant` between the
    /// grant and the call.
    async fn record_or_replay_blocking_resource_then<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        blocking_resource: ResourceID,
        blocked_signal_mask: Option<u64>,
        blocked_signal_mask_unknown: bool,
        after_grant: impl FnOnce(&mut G),
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let op_id = match &blocking_resource {
            ResourceID::BlockingExternalIO(op_id) | ResourceID::BlockingRtSigsuspend(op_id) => {
                *op_id
            }
            _ => unreachable!("blocking syscall helper requires a blocking resource"),
        };
        // Replay serves some of these calls from the recording, so they cannot block.
        // They still run in the background, as they did during recording, so that the
        // schedule matches the recording's. The flag makes the scheduler commit no
        // other turn until they finish. Otherwise a stand-in descriptor that replay
        // creates for an accepted connection could take its slot before or after
        // another thread's open or close, and which of these calls had finished, and
        // so the order in which they rejoin, would depend on host timing
        // (https://github.com/rrnewton/hermit/pull/3871).
        let replay_served_from_log = self.cfg.replaying
            && (replay_serves_blocking_call_from_log(call)
                || replay_serves_socket_call_from_log(guest, call));
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
            "record_or_replay_blocking (BlockingExternalIO) reached for an internal fd \
             (pipe or socketpair endpoint) on syscall {}; internal fds must use the \
             InternalIOPolling path",
            call.name()
        );
        {
            let mut rsrcs = Resources::new(dettid);
            // With sequentialization enabled, only truly EXTERNAL endpoints reach here.
            // Without it, resource_request is a no-op and internal fds may block directly.
            rsrcs.insert(blocking_resource, Permission::RW);
            rsrcs.fyi(call.name());
            rsrcs.blocked_signal_mask = blocked_signal_mask;
            rsrcs.blocked_signal_mask_unknown = blocked_signal_mask_unknown;
            rsrcs.replay_served_from_log = replay_served_from_log;
            resource_request(guest, rsrcs).await;
        }
        after_grant(guest);
        tracing::trace!(
            "Guest proceeding to execute potentially blocking call {}...",
            call.name()
        );
        let res = self
            .record_or_replay_preserving_tool_errors(guest, call)
            .await;
        // N.B. BlockingExternalIO is a "oneshot" resource, so no need to release
        // explicitly here:
        {
            let mut rsrcs = Resources::new(dettid);
            rsrcs.insert(ResourceID::BlockedExternalContinue(op_id), Permission::RW);
            rsrcs.fyi(call.name());
            resource_request(guest, rsrcs).await;
        }
        res
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

        let action = match ioaction_based_on_fd_status(guest, call) {
            Ok(action) => action,
            Err(errno) => {
                // Descriptor metadata is advisory for choosing an execution strategy. If the
                // descriptor is invalid or cannot be classified, execute through the
                // scheduler-safe blocking path and let the kernel provide the syscall errno.
                // Returning the metadata error (or panicking on it) can change Linux error
                // precedence, for example connect(-1, invalid_sockaddr, ...).
                tracing::trace!(
                    "NonblockableSyscall: fd classification failed with {}; executing kernel-authoritatively: {}",
                    errno,
                    call.name()
                );
                return self.record_or_replay_blocking(guest, wrapped).await;
            }
        };

        // Is this operation on a container-INTERNAL fd (a pipe or a socketpair
        // endpoint)? Internal fds are made physically nonblocking even in
        // record/replay (see handle_pipe2 and handle_socketpair), so they can take the
        // deterministic InternalIOPolling nonblockize-and-retry path. They must NOT be
        // forced onto the BlockingExternalIO path in R/R: a reader and its paired
        // writer are not independent, so descheduling the reader as "external
        // blocking IO" deadlocks the sequentialized scheduler (the documented R/R pipe
        // hang), and a one-shot read on the physically nonblocking fd returns EAGAIN.
        // Truly external endpoints (host fds, network sockets) still use
        // BlockingExternalIO. Other sockets are left external for now: there is no
        // internal-vs-external socket detection yet (see the handle_accept4 comment).
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
            // In record/replay mode, route an internal-fd (pipe or socketpair) read/write through the
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
            self.record_or_replay_preserving_tool_errors(guest, wrapped)
                .await
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
        expected_open_file: OpenFileId,
    ) -> Result<i64, Error> {
        const MAX_IOVECS: usize = 1024;
        // Linux limits a single vectored transfer to INT_MAX rounded down to a page.
        const MAX_RW_COUNT: usize = 0x7fff_f000;
        // Linux guarantees pipe writes through this size are atomic.
        const PIPE_BUF: usize = 4096;
        // Every backend provides at least 512 bytes of tool scratch. Linux's
        // own fast-iovec path is smaller; this covers common vectors.
        const STACK_IOVECS: usize = 32;

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
        let mut resources = pipe_writev_resources(guest.thread_state().dettid, call);
        let subtool = self.cfg.recordreplay_modes.then_some(self);
        let mut current = Syscall::Writev(call);
        let mut written_total = 0usize;

        // Keep ordinary signals pending while the scheduler and the target-side
        // disposition check decide whether a wakeup interrupts this write. The
        // ptrace-owned backends need the explicit mask while inspecting the
        // target's current disposition.
        let blocked_mask = blocked_signal_mask();
        let mut stack = guest.stack().await;
        let atomic_scratch_iov = if atomic_pipe_write && iovecs.len() <= STACK_IOVECS {
            let mut raw_iovecs = [libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            }; STACK_IOVECS];
            for (raw, (base, length)) in raw_iovecs.iter_mut().zip(&iovecs) {
                raw.iov_base = *base as *mut libc::c_void;
                raw.iov_len = *length;
            }
            let scratch_iov: Addr<libc::iovec> = stack.push(raw_iovecs).cast();
            Some(scratch_iov.as_raw())
        } else {
            None
        };
        let blocked_mask_addr = stack.push(blocked_mask);
        let old_mask_addr = stack.reserve::<KernelSigset>();
        let action_addr = stack.reserve::<KernelSigaction>();
        let _mask_guard = stack.commit()?;
        let guest_signal_mask =
            block_signals_for_disposition(guest, blocked_mask_addr, old_mask_addr).await?;

        let result: Result<i64, Error> = loop {
            // A cross-task signal can replace the initial scheduler request,
            // before the first physical attempt, as well as a later retry.
            let status = resource_request(guest, resources.clone()).await;
            let disposition = match wait_signal_disposition(
                guest,
                status.clone(),
                &guest_signal_mask,
                action_addr,
                true,
            )
            .await
            {
                Ok(disposition) => disposition,
                Err(error) => break Err(error),
            };
            if matches!(status, ResumeStatus::Signaled(_))
                && let Some(result) = interrupted_write_result(&call, written_total, disposition)
            {
                break result;
            }

            if resources.poll_attempt > 0
                && !guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| {
                        detfd.open_file_id() == expected_open_file
                    })
                    .unwrap_or(false)
            {
                break if written_total > 0 {
                    Ok(written_total as i64)
                } else {
                    self.refuse_unserviceable_operation(guest, Sysno::writev, Errno::EOPNOTSUPP)
                        .await
                };
            }

            let result = if atomic_pipe_write {
                self.execute_atomic_pipe_writev_attempt(guest, call, &iovecs, atomic_scratch_iov)
                    .await
            } else {
                match subtool {
                    Some(detcore) => {
                        detcore
                            .record_or_replay_preserving_tool_errors(guest, current)
                            .await
                    }
                    None => guest.inject_with_retry(current).await.map_err(Error::from),
                }
            };
            match result {
                Ok(written) if written > 0 => {
                    let written = match usize::try_from(written) {
                        Ok(written) => written,
                        Err(_) => break Err(Errno::EIO.into()),
                    };
                    written_total = match written_total.checked_add(written) {
                        Some(written_total) => written_total,
                        None => break Err(Errno::EIO.into()),
                    };
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
                Err(Error::Errno(Errno::EAGAIN)) => {
                    if !atomic_pipe_write && matches!(current, Syscall::Writev(_)) {
                        current = match remaining_writev_segment(call.fd(), &iovecs, 0, target) {
                            Ok(Some(write)) => Syscall::Write(write),
                            Ok(None) => break Ok(0),
                            Err(error) => break Err(error.into()),
                        };
                    }
                }
                Err(error) => {
                    break finish_partial_record_or_replay_write(written_total as i64, error);
                }
                Ok(_) => break Err(Errno::EIO.into()),
            }

            resources.poll_attempt += 1;
            tracing::trace!(
                "Retry #{} for {}blocking pipe writev after EAGAIN: {}",
                resources.poll_attempt,
                if atomic_pipe_write { "atomic " } else { "" },
                call.display(&guest.memory())
            );
            record_retry_event(guest, call).await;
        };

        restore_signals_after_disposition(guest, old_mask_addr).await?;
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2176): Review scalar blocking-pipe write completion and
    // the fail-closed descriptor-replacement boundary.
    /// Complete a logically blocking scalar pipe write after Hermit has made the pipe
    /// physically nonblocking.
    ///
    /// Linux may return a positive short write for a request larger than `PIPE_BUF`, then
    /// continue blocking for the remainder. Hermit's physical `O_NONBLOCK` is an internal
    /// scheduler mechanism, so exposing that first short write changes guest behavior. Retry
    /// the unconsumed suffix until the logical write completes, a signal arrives, or a real
    /// error occurs. A signal or error after progress returns the partial byte count, matching
    /// Linux.
    ///
    /// A concurrent close/dup2 can replace the numeric fd while this helper is yielded. Linux
    /// keeps the original open-file description alive inside a blocking syscall, but Reverie
    /// does not yet expose a backend-neutral retained-fd handle. Detect replacement before a
    /// retry and fail closed rather than writing the suffix into an unrelated object.
    pub async fn execute_blocking_pipe_write<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Write,
        expected_open_file: OpenFileId,
    ) -> Result<i64, Error> {
        const MAX_RW_COUNT: usize = 0x7fff_f000;

        let target = call.len().min(MAX_RW_COUNT);
        if target == 0 {
            return self.execute_nonblockable_fd_syscall(guest, call).await;
        }

        tracing::trace!(
            "NonblockableSyscall: converting to nonblocking syscall (internal polling): write"
        );
        let mut resources = Resources::new(guest.thread_state().dettid);
        resources.insert(ResourceID::InternalIOPolling, Permission::W);
        resources.fyi(call.name());
        // Interruptible by a cross-task signal the kernel would deliver, as a
        // polled read is (`retry_nonblocking_syscall_helper`): the scheduler
        // answers the next request `Signaled`, and the write returns the
        // bytes it wrote, or ERESTARTSYS before any
        // (<https://github.com/rrnewton/hermit/issues/4187>).
        resources.set_signal_interrupt_errno(call.signal_interrupt_errno());
        resources.signal_state_task = Some((guest.pid().as_raw(), guest.tid().as_raw()));
        let subtool = self.cfg.recordreplay_modes.then_some(self);
        let mut current = call;
        let mut written_total = 0usize;

        loop {
            // A signal ends the write only if it would still wait for room:
            // the attempt below runs first, as Linux's `pipe_write` checks
            // for a pending signal only when it must wait. After the signal,
            // the bytes written so far are returned, or the restart errno if
            // there are none.
            let signaled = resources.poll_attempt > 0
                && matches!(
                    resource_request(guest, resources.clone()).await,
                    ResumeStatus::Signaled(_)
                );

            if resources.poll_attempt > 0
                && !guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| {
                        detfd.open_file_id() == expected_open_file
                    })
                    .unwrap_or(false)
            {
                break if written_total > 0 {
                    Ok(written_total as i64)
                } else {
                    self.refuse_unserviceable_operation(guest, Sysno::write, Errno::EOPNOTSUPP)
                        .await
                };
            }

            let result = match subtool {
                Some(detcore) => {
                    detcore
                        .record_or_replay_preserving_tool_errors(guest, current)
                        .await
                }
                None => guest.inject_with_retry(current).await.map_err(Error::from),
            };
            // As in `retry_nonblocking_syscall_helper`, an attempt that the
            // pending signal stopped under record and replay is not made again.
            match result {
                Ok(written) if written > 0 => {
                    let written = usize::try_from(written).map_err(|_| Errno::EIO)?;
                    let remaining = target.checked_sub(written_total).ok_or(Errno::EIO)?;
                    if written > remaining {
                        break Err(Errno::EIO.into());
                    }
                    written_total = written_total.checked_add(written).ok_or(Errno::EIO)?;
                    if written_total == target || signaled {
                        break Ok(written_total as i64);
                    }
                    let Some(buffer) = call.buf() else {
                        break Err(Errno::EFAULT.into());
                    };
                    let Some(next_buffer) = buffer
                        .as_raw()
                        .checked_add(written_total)
                        .and_then(Addr::<u8>::from_raw)
                    else {
                        break finish_partial_record_or_replay_write(
                            written_total as i64,
                            Errno::EFAULT.into(),
                        );
                    };
                    current = call
                        .with_buf(Some(next_buffer))
                        .with_len(target - written_total);
                }
                Ok(0) => break Ok(written_total as i64),
                Err(Error::Errno(Errno::EAGAIN)) if signaled => {
                    break if written_total > 0 {
                        Ok(written_total as i64)
                    } else {
                        Err(call.signal_interrupt_errno().into())
                    };
                }
                Err(Error::Errno(Errno::EAGAIN)) => {}
                Err(error) => {
                    break finish_partial_record_or_replay_write(written_total as i64, error);
                }
                Ok(_) => break Err(Errno::EIO.into()),
            }

            resources.poll_attempt += 1;
            tracing::trace!(
                "Retry #{} for blocking pipe write: {}",
                resources.poll_attempt,
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
        stack_iov: Option<usize>,
    ) -> Result<i64, Error> {
        if let Some(stack_iov) = stack_iov {
            let scratch_call = call.with_iov(Addr::from_raw(stack_iov));
            return if self.cfg.recordreplay_modes {
                self.record_or_replay_preserving_tool_errors(guest, scratch_call)
                    .await
            } else {
                guest
                    .inject_with_retry(scratch_call)
                    .await
                    .map_err(Error::from)
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
            self.record_or_replay_preserving_tool_errors(guest, scratch_call)
                .await
        } else {
            guest
                .inject_with_retry(scratch_call)
                .await
                .map_err(Error::from)
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

/// Returns the strategy for an FD-based call that may block when executed.
///
/// Failure means descriptor metadata could not classify the call; the caller must preserve the
/// kernel's authority over the syscall result rather than exposing this advisory lookup error.
pub fn ioaction_based_on_fd_status<
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
    C: SyscallInfo + Into<Syscall>,
>(
    guest: &mut G,
    call: C,
) -> Result<IOAction, Errno> {
    let wrapped: Syscall = call.into();
    let fd = get_fd(wrapped).unwrap_or_else(|| panic!("Failed to get fd for {}", call.name()));
    let (phys, virt) = guest.thread_state().with_detfd(fd, |detfd| {
        (detfd.physically_nonblocking(), detfd.is_nonblocking())
    })?;
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
        Ok(IOAction::Blocking)
    } else if virt && phys {
        // TT: both nonblocking, so firing once is sufficient
        Ok(IOAction::PassThru)
    } else {
        // FT: Need to simulate blocking on top of nonblocking.
        Ok(IOAction::NonblockizeRetry)
    }
}

/// Does this single-fd syscall operate on a container-INTERNAL file descriptor?
///
/// This recognizes pipes, whose two endpoints are always both owned by guest
/// processes inside the deterministic container, and `socketpair(2)` endpoints, which
/// have that same property for the same reason. Internal fds are made physically
/// nonblocking (see `handle_pipe2` and `handle_socketpair`) so a potentially-blocking
/// op on them can use the deterministic `InternalIOPolling` nonblockize-and-retry
/// strategy instead of `BlockingExternalIO`. Treating an internal fd as external
/// blocking IO deadlocks the sequentialized scheduler in record/replay, because a
/// reader and its paired writer are not independent.
///
/// A socketpair is the one socket case where internality is known at creation time:
/// the kernel creates BOTH ends, so no peer outside the container can ever hold one.
/// That is why it does not need the general internal-vs-external socket detection
/// (loopback / AF_UNIX-to-another-guest vs a real host peer) that remains unsolved;
/// every OTHER socket still conservatively remains external.
///
/// The residual assumption, stated because it is an assumption and not a proof: a
/// guest could pass a socketpair endpoint out of the container over `SCM_RIGHTS` to a
/// process on the host. A pipe fd can escape by exactly the same route and this code
/// has always accepted that for pipes, so recognizing socketpairs widens no exposure
/// class -- but neither claim is airtight.
///
/// The converse gap: a socketpair endpoint or pipe that a guest RECEIVES over
/// `SCM_RIGHTS` is untracked by Detcore, so it is not recognized here and keeps the
/// kernel-authoritative external path. Its open file description is still physically
/// nonblocking, so a blocking read through the received descriptor can return EAGAIN
/// under record, as it already could for a received pipe.
///
/// Syscalls whose fd is not directly extractable (e.g. poll/ppoll, which carry a
/// pointer to an fd array) return false and keep their existing handling.
pub fn syscall_targets_internal_fd<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &mut G,
    call: Syscall,
) -> bool {
    // An epoll wait's descriptor is the epoll set it waits on, never a pipe it
    // reads; a guest that passes a pipe gets EINVAL from the kernel.
    if matches!(call, Syscall::EpollWait(_) | Syscall::EpollPwait(_)) {
        return false;
    }
    match get_fd(call) {
        Some(fd) => guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                matches!(detfd.ty(), FdType::Pipe) || detfd.is_socketpair_endpoint()
            })
            .unwrap_or(false),
        None => false,
    }
}

/// Does replay answer this call from the recording rather than by running it?
///
/// This follows the replayer's dispatch. It serves `accept`, `accept4`, `connect`,
/// `sendto`, `sendmsg`, `recvfrom` and `recvmsg` from its log whatever the descriptor
/// is, so they count even on a descriptor Detcore does not track, such as a listener
/// received over `SCM_RIGHTS`; an untagged accept there would let its stand-in take a
/// slot at a host-timed point. `read`, `readv`, `write` and `writev` count only on a
/// socket Detcore created with `socket(2)` or `accept(2)`: on other descriptors,
/// replay may run them for real as eventfd operations, which can block, so an unknown
/// descriptor is not assumed to be served from the log. `recvmmsg` and `sendmmsg` are
/// recorded by neither recorder nor replayer (on an accepted connection record/replay
/// refuses them, see `refuse_log_served_mmsg`).
fn replay_serves_socket_call_from_log<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &mut G,
    call: Syscall,
) -> bool {
    let fd = match call {
        Syscall::Accept(_)
        | Syscall::Accept4(_)
        | Syscall::Connect(_)
        | Syscall::Sendto(_)
        | Syscall::Sendmsg(_)
        | Syscall::Recvfrom(_)
        | Syscall::Recvmsg(_) => return true,
        Syscall::Read(s) => s.fd(),
        Syscall::Readv(s) => s.fd(),
        Syscall::Write(s) => s.fd(),
        Syscall::Writev(s) => s.fd(),
        _ => return false,
    };
    is_log_served_socket(guest, fd)
}

/// Does replay answer this otherwise blocking call from the recording alone?
///
/// The replayer serves `poll`, `ppoll`, `select` and `pselect6` from the log
/// without running them. `epoll_wait` is excluded because replay reapplies its
/// side effects on live descriptors, which may block.
fn replay_serves_blocking_call_from_log(call: Syscall) -> bool {
    matches!(
        call,
        Syscall::Poll(_) | Syscall::Ppoll(_) | Syscall::Select(_) | Syscall::Pselect6(_)
    )
}

/// Is `fd` a socket whose connection record and replay keep only in the log?
///
/// That is a socket Detcore created with `socket(2)` or `accept(2)`, but not a
/// socketpair endpoint: replay recreates a socketpair, and its operations run
/// live.
fn is_log_served_socket<G: Guest<Detcore<T>>, T: RecordOrReplay>(guest: &mut G, fd: i32) -> bool {
    guest
        .thread_state()
        .with_detfd(fd, |detfd| {
            matches!(detfd.ty(), FdType::Socket) && !detfd.is_socketpair_endpoint()
        })
        .unwrap_or(false)
}

/// Is `fd` a connection that `accept(2)` or `accept4(2)` returned? Record and
/// replay keep such a connection only in the log, and replay stands in an
/// eventfd for it, so an operation that runs live during replay meets that
/// eventfd rather than a connected socket.
pub(crate) fn is_accepted_connection<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &mut G,
    fd: i32,
) -> bool {
    guest
        .thread_state()
        .with_detfd(fd, |detfd| detfd.is_accepted_connection())
        .unwrap_or(false)
}

/// A large subset of system calls have a single, unique file descriptor argument.  This
/// is a convenience function for grabbing that argument.
///
/// It does not cover system calls with multiple fd arguments, with pointers to heap
/// structures that contain fds.
pub(crate) fn get_fd(s: Syscall) -> Option<i32> {
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
// `async_trait` rewrites `into_nonblocking` to return a boxed future and marks it
// `#[must_use]`; the future is already `#[must_use]` in its own right, so clippy sees a double
// annotation. Both are generated, so the lint's own suggestion -- give the `must_use` an explicit
// reason -- cannot be applied at this source. Allowed at the item rather than crate-wide so any
// hand-written double `must_use` elsewhere still fails `#![deny(clippy::all)]` (detcore/src/lib.rs:35).
// Appeared with nightly-2026-08-08 (rustc 1.99.0-nightly 1a98b1e13) against an unchanged tree.
#[allow(clippy::double_must_use)]
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

    /// Return the errno for an interrupting signal that ends this wait after it began,
    /// on a backend that decides interruption from the kernel's signal state
    /// (`backend_supports_blocked_wait_signal_interruption`). The kernel applies the
    /// guest's disposition to it on resume, so it must be the restart code Linux uses
    /// for this call: a handler then turns it into `EINTR` or a restart, and a signal
    /// with no handler restarts the call.
    fn kernel_restart_errno(&self) -> Errno {
        self.signal_interrupt_errno()
    }

    /// Whether Linux restarts this call, after [`kernel_restart_errno`](Self::kernel_restart_errno)
    /// with no handler, through a restart block that keeps its absolute deadline: a
    /// `poll` with a positive timeout (`do_restart_poll`) and a timed `FUTEX_WAIT` or
    /// `FUTEX_WAIT_BITSET` (`futex_wait_restart`). Its restart code is then
    /// `ERESTART_RESTARTBLOCK`, and
    /// Detcore keeps the deadline in a [`RestartBlock`] for the `restart_syscall` that
    /// the kernel runs next (<https://github.com/rrnewton/hermit/issues/3358>).
    ///
    /// Such a wait is also not ended by a default job-control stop signal
    /// (`KernelSignalState::interrupting_wait`), so that a stop Linux discards in an
    /// orphaned process group does not end the call before its deadline.
    fn restart_keeps_deadline(&self) -> bool {
        false
    }

    /// Signals the wait itself accepts rather than being interrupted by, read from
    /// guest memory. Only `rt_sigtimedwait` has any.
    fn signals_consumed_by_wait<M: MemoryAccess>(&self, _memory: &M) -> KernelSigset {
        0
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

fn interrupted_write_result<C: NonblockableSyscall>(
    call: &C,
    written_total: usize,
    disposition: Option<WaitSignalDisposition>,
) -> Option<Result<i64, Error>> {
    match disposition {
        None => None,
        Some(_) if written_total > 0 => Some(Ok(written_total as i64)),
        Some(_) => Some(Err(call.signal_interrupt_errno().into())),
    }
}

fn pipe_writev_resources(dettid: DetTid, call: reverie::syscalls::Writev) -> Resources {
    let mut resources = Resources::new(dettid);
    resources.insert(ResourceID::InternalIOPolling, Permission::W);
    resources.fyi(call.name());
    resources.set_signal_interrupt_errno(call.signal_interrupt_errno());
    // Polls with every ordinary signal blocked and checks the target's
    // disposition itself after a `Signaled` wake, so the scheduler's drain
    // must not filter by the kernel mask (`execute_blocking_pipe_writev`).
    resources.decides_signal_interruption_itself = true;
    resources
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

    /// Linux ends an interrupted `poll` with a restart code that a handler turns into
    /// `EINTR` and that restarts the call when no handler runs.
    ///
    /// With a positive timeout that code is Linux's own, `ERESTART_RESTARTBLOCK`: the
    /// kernel then runs `restart_syscall`, which Detcore resumes with the deadline it
    /// kept ([`RestartBlock`]), as `do_restart_poll` does
    /// (https://github.com/rrnewton/hermit/issues/3358). An infinite timeout has no
    /// deadline to keep, so it returns `ERESTARTNOHAND`, and running `poll` again with
    /// its original arguments is the same restart.
    fn kernel_restart_errno(&self) -> Errno {
        if self.restart_keeps_deadline() {
            Errno::ERESTART_RESTARTBLOCK
        } else {
            Errno::ERESTARTNOHAND
        }
    }

    /// A zero timeout never blocks, and a negative one never expires.
    fn restart_keeps_deadline(&self) -> bool {
        self.timeout() > 0
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

    /// Linux ends an interrupted `ppoll` with a restart code that a handler turns into
    /// `EINTR` and that restarts the call when no handler runs.
    fn kernel_restart_errno(&self) -> Errno {
        Errno::ERESTARTNOHAND
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

// `epoll_pwait` was the one member of the
// poll/epoll family with no nonblocking form, even though `Ppoll` -- the
// sigmask variant of `poll` -- has had one all along. Programs that issue
// `epoll_pwait` DIRECTLY (libuv does, which is how the `cmake` hang surfaced)
// therefore reached an unhandled path while the `EpollWait` impl above went
// unused by them. Note this is NOT glibc's `epoll_wait(2)` on x86_64: glibc
// only spells it `epoll_pwait` where `__NR_epoll_wait` is absent, which x86_64
// is not. With a NULL sigmask the two calls are semantically identical, so the
// nonblocking form is the same: timeout 0, EINTR, and a 0 (no events) timeout
// return.
#[async_trait]
impl NonblockableSyscall for reverie::syscalls::EpollPwait {
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

impl TimeoutableSyscall for reverie::syscalls::EpollPwait {
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

    /// Linux restarts an untimed `FUTEX_WAIT` under `SA_RESTART` (`-ERESTARTSYS`), but a
    /// timed wait returns `-ERESTART_RESTARTBLOCK`, which a handler always turns into
    /// `EINTR` (https://github.com/rrnewton/hermit/issues/3146).
    ///
    /// A timed `FUTEX_WAIT` or `FUTEX_WAIT_BITSET` returns Linux's code: with no
    /// handler the kernel then runs `restart_syscall`, which Detcore resumes with the
    /// deadline it kept ([`RestartBlock`]), as `futex_wait_restart` does
    /// (https://github.com/rrnewton/hermit/issues/3358). Linux's `futex_wait` keeps
    /// the absolute deadline of every timed wait in the restart block, so a
    /// `FUTEX_WAIT_BITSET` deadline the guest moves in memory after the wait began
    /// does not change when the restarted wait ends. `FUTEX_WAIT_BITSET` used to
    /// return `ERESTARTNOHAND`, which ran the call again with its original
    /// arguments and so read the deadline again (round-10 finding Medium 4 on
    /// https://github.com/rrnewton/hermit/pull/3361).
    fn kernel_restart_errno(&self) -> Errno {
        if self.restart_keeps_deadline() {
            Errno::ERESTART_RESTARTBLOCK
        } else if self.timeout().is_some() {
            Errno::ERESTARTNOHAND
        } else {
            Errno::ERESTARTSYS
        }
    }

    /// Every timed wait: `FUTEX_WAIT`'s relative timeout and `FUTEX_WAIT_BITSET`'s
    /// absolute deadline both become the absolute deadline Linux's restart keeps.
    fn restart_keeps_deadline(&self) -> bool {
        matches!(
            self.futex_op() & libc::FUTEX_CMD_MASK,
            libc::FUTEX_WAIT | libc::FUTEX_WAIT_BITSET
        ) && self.timeout().is_some()
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

    /// Signals in the wait's own set are accepted by it, not interrupting. A set
    /// that cannot be read counts as empty, which nothing observes: the wait's
    /// first probe runs before any pending signal is considered
    /// (`KernelSignalWait::inject_first_probe`) and fails with `EFAULT`, as Linux
    /// fails the call when it cannot copy the set.
    fn signals_consumed_by_wait<M: MemoryAccess>(&self, memory: &M) -> KernelSigset {
        self.set()
            .and_then(|set| memory.read_value(set.cast::<KernelSigset>()).ok())
            .unwrap_or(0)
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
// TODO-HUMAN-REVIEW(#794)
/// Vectored reads have the same blocking behavior as scalar reads on pipes and sockets.
#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Readv {
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

/// What a wait that ended with `ERESTART_RESTARTBLOCK` keeps for the
/// `restart_syscall` that Linux runs next when no handler runs
/// (`NonblockableSyscall::restart_keeps_deadline`). Linux keeps the call and its
/// absolute deadline in the thread's restart block (`do_restart_poll`,
/// `futex_wait_restart`). Detcore emulates the wait, so the kernel's restart block
/// does not describe it, and Detcore keeps its own
/// (https://github.com/rrnewton/hermit/issues/3358).
///
/// The record lives until the thread's next syscall, which takes it
/// (`Detcore::handle_syscall_event`). Other first syscalls, sigreturn and exec
/// discard the record; separate threads and newly initialized children have
/// independent records. A caught handler does not discard it: Linux resets its
/// restart block at sigreturn (`restore_sigcontext`), not when it sets up the
/// handler, so a handler whose first syscall is `restart_syscall` resumes the
/// interrupted wait with the copied arguments and deadline. Detcore does the
/// same when that call stops at the interrupted call's address, as it does
/// through libc's `syscall()` (the `restartfirst` mode of the
/// `external_signal_interrupt` guest). Linux also keeps its restart block across
/// the handler's other syscalls and resumes it from any address; Detcore does
/// not, so a `restart_syscall` after another syscall or at another address takes
/// the unsupported-syscall policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RestartBlock {
    /// The instruction pointer at the interrupted call's syscall stop: the address
    /// after its `syscall` instruction. The kernel's restart moves the instruction
    /// pointer back onto that instruction with `restart_syscall` in `rax`, so the
    /// restart stops at the same address.
    pub(crate) rip: u64,
    /// The wait's absolute deadline.
    pub(crate) deadline: Option<LogicalTime>,
    /// The interrupted call.
    pub(crate) call: RestartCall,
}

/// A call whose restart keeps its deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RestartCall {
    Poll(syscalls::Poll),
    Futex(syscalls::Futex),
}

impl RestartBlock {
    /// The record, if a `restart_syscall` that stopped at `rip` is the kernel's
    /// restart of the interrupted call. At any other address the guest made the
    /// call itself.
    pub(crate) fn resumed_at(self, rip: u64) -> Option<Self> {
        (self.rip == rip).then_some(self)
    }
}

/// Keeps `deadline` for the kernel's restart of `call` when the wait ended with
/// `ERESTART_RESTARTBLOCK` (`RestartBlock`).
pub(crate) async fn keep_restart_block<T, G>(
    guest: &mut G,
    result: &Result<i64, Error>,
    call: RestartCall,
    deadline: Option<LogicalTime>,
) where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if matches!(result, Err(Error::Errno(errno)) if *errno == Errno::ERESTART_RESTARTBLOCK) {
        let rip = guest.regs().await.rip;
        let block = RestartBlock {
            rip,
            deadline,
            call,
        };
        tracing::trace!(
            "[tid {}] the wait keeps {:?} for its restart",
            guest.tid(),
            block
        );
        guest.thread_state_mut().restart_block = Some(block);
    }
}

/// The restart code that ends a wait for a signal the backend holds and delivers
/// when the guest resumes. The backend passes a held signal on without reporting it
/// to the tool when the call returns `ERESTART_RESTARTBLOCK`, so for any signal but
/// `SIGSTOP`, which it never reports, the wait returns `ERESTARTNOHAND` instead. A
/// handler turns either code into `EINTR`, and a signal that kills the process
/// leaves no restart. The difference is a stop, which `SIGSTOP` alone can be here:
/// a default job-control stop does not end a wait whose restart keeps its deadline
/// (`KernelSignalState::interrupting_wait`).
fn held_signal_restart_errno(restart_errno: Errno, signal: i32) -> Errno {
    if restart_errno == Errno::ERESTART_RESTARTBLOCK && signal != libc::SIGSTOP {
        Errno::ERESTARTNOHAND
    } else {
        restart_errno
    }
}

/// The deadline a polling request carries so the scheduler can restore a
/// backed-off poller once committed virtual time passes it
/// (`Resources::poll_deadline`, <https://github.com/rrnewton/hermit/issues/3952>).
/// `None` under record/replay, whose schedule must not change.
pub(crate) fn poll_deadline_for<T, G>(
    guest: &G,
    deadline: Option<LogicalTime>,
) -> Option<LogicalTime>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if guest.config().recordreplay_modes {
        None
    } else {
        deadline
    }
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
    if guest
        .config()
        .backend_supports_blocked_wait_signal_interruption
    {
        return retry_blocking_wait_with_kernel_signal_state(guest, call, rsrc, maybe_tup).await;
    }
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
    // The signals the wait takes from the queue (rt_sigtimedwait's set).
    let consumed = call0.signals_consumed_by_wait(&guest.memory());
    // The stack-allocated memory here needs to live across the loop, which means
    // surviving multiple syscall injections:
    let (call, _maybe_stackguard) = call0.into_nonblocking(guest).await;
    let mut rsrc = rsrc.clone();
    rsrc.poll_deadline = poll_deadline_for(guest, maybe_timeout.map(|(deadline, _)| deadline));
    // A restartable call parked in internal IO polling, such as a `read` of an
    // empty pipe, is interrupted by a signal another task sends, as a parked
    // pipe write is (`pipe_writev_resources`). The marker lets the scheduler
    // record the signal for it (`Scheduler::notify_signal_pending`) and answer
    // its next turn `Signaled`; the call then returns `ERESTARTSYS`, and the
    // kernel applies the guest's disposition when it resumes: a handler runs
    // and the call fails with `EINTR` or restarts under `SA_RESTART`. Without
    // it, the poll never saw the signal and waited forever
    // (<https://github.com/rrnewton/hermit/issues/4187>).
    if rsrc.signal_interrupt_errno().is_none()
        && rsrc.resources.len() == 1
        && rsrc.resources.contains_key(&ResourceID::InternalIOPolling)
        && call.signal_interrupt_errno() == Errno::ERESTARTSYS
    {
        rsrc.set_signal_interrupt_errno(Errno::ERESTARTSYS);
        rsrc.signal_state_task = Some((guest.pid().as_raw(), guest.tid().as_raw()));
    }

    loop {
        let resumed = match call.into() {
            Syscall::Read(read) if maybe_timeout.is_none() && _maybe_stackguard.is_none() => {
                crate::tool_global::polled_read_request(guest, read, rsrc.clone()).await
            }
            _ => resource_request(guest, rsrc.clone()).await,
        };
        // A signal ends the call only if it would still block, as Linux
        // checks for a pending signal only when the call must wait: a byte
        // written, or the last writer closed, before the signal is read
        // first. The probe below goes through the record/replay path like
        // every attempt (Codex review of
        // https://github.com/rrnewton/hermit/pull/4198).
        let signaled = matches!(resumed, ResumeStatus::Signaled(_));
        // A pending real-time timer expiration in the set this wait takes
        // (https://github.com/rrnewton/hermit/issues/4089).
        let pending = pending_real_time_among(guest, consumed).await;
        act_on_pending_real_time(guest, pending, consumed, 0).await;
        // Route through the record/replay subtool for internal pipes so each poll (an
        // EAGAIN, or the final data-bearing read) becomes one recorded event that replay
        // reproduces deterministically; otherwise execute the syscall directly.
        let res = match subtool {
            Some(detcore) => {
                detcore
                    .record_or_replay_preserving_tool_errors(guest, call)
                    .await
            }
            None => guest.inject_with_retry(call).await.map_err(Error::from),
        };
        // Under record and replay the attempt is a plain injection, which the
        // signal that woke this call, still pending, stops before it runs
        // (ERESTARTSYS, not run): the call then returns that restart errno,
        // as on main, though a byte, end of file or room may have been ready.
        // Making the attempt again would replace the held signal with any
        // other pending one (https://github.com/rrnewton/reverie/issues/1012),
        // and record and replay could not agree on when it is safe.
        let syscall_result = match res {
            Ok(value) => Ok(value),
            Err(Error::Errno(error)) => Err(error),
            Err(error) => return Err(error),
        };
        if signaled && call.syscall_would_have_blocked(syscall_result) {
            let errno = call.signal_interrupt_errno();
            tracing::trace!(
                "retry_nonblocking_syscall: interrupted by signal while {} would block: {:?}",
                call.display(&guest.memory()),
                errno
            );
            return Err(errno.into());
        }
        if call.syscall_would_have_blocked(syscall_result) {
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
                        syscall_result,
                        timeout - new_time,
                        call.display(&guest.memory())
                    );
                    record_retry_event(guest, call).await;
                }
            } else {
                tracing::trace!(
                    "Retry #{} for syscall due to result {:?}: {}",
                    rsrc.poll_attempt,
                    syscall_result,
                    call.display(&guest.memory())
                );
                record_retry_event(guest, call).await;
            }
        } else {
            let res = call
                .normalize_nonblocking_result(syscall_result, rsrc.poll_attempt > 0)
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

/// Retry a blocking wait on a backend that decides signal interruption from the
/// kernel's signal state (`backend_supports_blocked_wait_signal_interruption`).
///
/// A wait ends early only for a signal that would end it natively: one the guest
/// does not block and that is caught, or whose default action terminates or stops
/// the process (https://github.com/rrnewton/hermit/issues/3146). Blocked,
/// ignored, and default-ignored signals do not end it, and neither does a default
/// job-control stop when Linux's restart of the call keeps its deadline
/// (`NonblockableSyscall::restart_keeps_deadline`). Such a signal that stops one
/// of the wait's injections is absorbed, and the wait runs on. A stop that
/// `KernelSignalWait::inject_absorbing` cannot absorb ends the wait with a
/// transparent restart when the backend holds nothing whose loss matters, and
/// otherwise with a [`HeldSignalRefusal`], which `refuse_held_signal_loss`
/// turns into the unsupported-operation refusal once the guest's mask is back.
///
/// The first probe runs under the guest's own mask, and before the first check
/// for a pending signal (`KernelSignalWait::inject_first_probe`): Linux reports a
/// descriptor that is ready when the call begins, a queued event, and an
/// argument error before it looks at pending signals, so the probe's result
/// stands whenever the signal arrived. If it would block, every blockable signal
/// is blocked for the rest of the wait, so later probes cannot be stopped by one
/// and every signal that arrives stays pending in the kernel, where `/proc`
/// reports it. Each turn classifies the pending set against the guest's mask and
/// dispositions, and an interrupting signal ends the wait with the call's restart
/// errno (see `KernelSignalWait::interrupted_with_state`). The guest's mask is
/// restored before returning, so the signal is delivered as the call returns.
/// The signals a scheduler resume names (`ResumeStatus::Signaled`): signals the
/// scheduler committed to the thread at that point.
fn committed_signals(resumed: &ResumeStatus) -> KernelSigset {
    match resumed {
        ResumeStatus::Signaled(Some(signals)) => signals.iter().fold(0, |set, signal| {
            set | kernel_sigset_bit(signal.0 as libc::c_int)
        }),
        _ => 0,
    }
}

async fn retry_blocking_wait_with_kernel_signal_state<T, G, C>(
    guest: &mut G,
    call0: C,
    rsrc: Resources,
    maybe_timeout: Option<(LogicalTime, Result<i64, Errno>)>,
) -> Result<i64, Error>
where
    C: NonblockableSyscall + Into<Syscall>,
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    retry_blocking_wait_under_mask(guest, call0, rsrc, maybe_timeout, None)
        .await
        .0
}

/// `retry_blocking_wait_with_kernel_signal_state` for a call that sleeps under
/// `wait_mask` instead of the guest's own mask, when it is set: the temporary
/// mask of an `epoll_pwait` (`KernelSignalWait::with_wait_mask`). `call0`
/// carries no mask, so its probes run under the guest's own mask until every
/// blockable signal is blocked, as for any other wait. Also returns whether the
/// signal that ended the wait must still be delivered under `wait_mask`: one
/// that only `wait_mask` lets in, pending in the kernel under the guest's own
/// mask once it is put back, which the caller delivers with
/// `inject_signal_delivery`, as a masked pselect6 does.
pub(crate) async fn retry_blocking_wait_under_mask<T, G, C>(
    guest: &mut G,
    call0: C,
    rsrc: Resources,
    maybe_timeout: Option<(LogicalTime, Result<i64, Errno>)>,
    wait_mask: Option<KernelSigset>,
) -> (Result<i64, Error>, bool)
where
    C: NonblockableSyscall + Into<Syscall>,
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let mut signals = KernelSignalWait::new(
        guest,
        call0.signals_consumed_by_wait(&guest.memory()),
        call0.restart_keeps_deadline(),
        call0.kernel_restart_errno(),
    )
    .with_deadline(maybe_timeout.is_some())
    .with_wait_mask(wait_mask);
    // Set when a signal that only `wait_mask` lets in ends the wait.
    let mut deliver_under_wait_mask = false;
    let mut rsrc = rsrc.clone();
    rsrc.poll_deadline = poll_deadline_for(guest, maybe_timeout.map(|(deadline, _)| deadline));
    let (mut call, mut guard) = call0.into_nonblocking(guest).await;
    let mut first_turn = true;
    // Set when only a signal the scheduler committed in this turn ends the
    // wait: the turn's probe runs first, and the wait ends with the restart
    // errno only if the call has nothing to report.
    let mut interrupt_after_probe;

    let result = loop {
        interrupt_after_probe = false;
        // A scheduler `Signaled` answer only says a signal may be pending. The kernel's
        // state below decides whether it ends the wait.
        let resumed = resource_request(guest, rsrc.clone()).await;
        // Read in this turn, before the probe: another thread may have armed a
        // host-timed source since the last turn, and none can until this turn ends.
        signals.hold_until_return(host_timed_signals(guest).await);
        // A `SIGCHLD` the scheduler committed to this thread in this turn, sent
        // at a commit point (a child's child-exit timer, or a POSIX timer whose
        // signal is `SIGCHLD`), ends the wait, as on Linux
        // (`KernelSignalWait::admit_committed`,
        // https://github.com/rrnewton/hermit/issues/4005).
        signals.admit_committed(committed_signals(&resumed) & kernel_sigset_bit(libc::SIGCHLD));
        // A pending real-time timer expiration that this wait would consume or
        // be ended by (https://github.com/rrnewton/hermit/issues/4089).
        let pending = pending_real_time_among(guest, REAL_TIME_SIGNALS).await;
        if pending != 0 {
            match signals.observable_now() {
                Ok(observable) => act_on_pending_real_time(guest, pending, observable, 0).await,
                Err(error) => break Err(error),
            }
        }
        let first = std::mem::take(&mut first_turn);
        // Never `inject_with_retry`: see `KernelSignalWait`.
        let injected = if first {
            // The first probe runs before the check, so a source that was ready when
            // the call began, or an argument error, is reported as Linux reports it
            // whatever signal is pending (`inject_first_probe`).
            signals.inject_first_probe(guest, call).await
        } else {
            let state = match signals.interrupted_with_state() {
                Ok((false, state)) => state,
                // Linux takes a completion before it returns EINTR: a queued
                // signal of rt_sigtimedwait's set (`do_sigtimedwait` dequeues
                // first, and again after it sleeps), a ready descriptor of poll
                // or epoll_wait. A signal the scheduler committed in this turn
                // was sent at the same fixed point as any completion this turn
                // can see, so the completion wins, as on Linux (Codex review of
                // https://github.com/rrnewton/hermit/pull/4039).
                Ok((true, state)) if signals.interrupts_only_by_committed(&state) => {
                    interrupt_after_probe = true;
                    deliver_under_wait_mask = signals.interrupts_only_under_wait_mask(&state);
                    state
                }
                Ok((true, state)) => {
                    deliver_under_wait_mask = signals.interrupts_only_under_wait_mask(&state);
                    let errno = call0.kernel_restart_errno();
                    tracing::trace!(
                        "retry_nonblocking_syscall: pending signals interrupt {}: {:?}",
                        call.display(&guest.memory()),
                        errno
                    );
                    break Err(errno.into());
                }
                Err(error) => break Err(error),
            };
            signals.inject_absorbing_after(guest, call, state).await
        };
        let syscall_result = match injected {
            Ok(result) => result,
            Err(error) => {
                tracing::trace!(
                    "retry_nonblocking_syscall: a signal stop ends the wait in {}: {:?}",
                    call.display(&guest.memory()),
                    error
                );
                break Err(error);
            }
        };
        if !call.syscall_would_have_blocked(syscall_result) {
            let res = call
                .normalize_nonblocking_result(syscall_result, rsrc.poll_attempt > 0)
                .map_err(|e| e.into());
            tracing::trace!(
                "retry_nonblocking_syscall: syscall completed after {} retries: {} = {:?}",
                rsrc.poll_attempt,
                call.display(&guest.memory()),
                res
            );
            break res;
        }
        if interrupt_after_probe {
            let errno = call0.kernel_restart_errno();
            tracing::trace!(
                "retry_nonblocking_syscall: a committed signal interrupts {}: {:?}",
                call.display(&guest.memory()),
                errno
            );
            break Err(errno.into());
        }
        if first {
            // The first turn's check, after its probe found nothing to report.
            match signals.interrupted_with_state() {
                Ok((false, _)) => {}
                Ok((true, state)) => {
                    deliver_under_wait_mask = signals.interrupts_only_under_wait_mask(&state);
                    let errno = call0.kernel_restart_errno();
                    tracing::trace!(
                        "retry_nonblocking_syscall: pending signals interrupt {}: {:?}",
                        call.display(&guest.memory()),
                        errno
                    );
                    break Err(errno.into());
                }
                Err(error) => break Err(error),
            }
        }
        if signals.needs_block() {
            // Only one scratch-stack guard may be live, so release the probe's, block
            // signals from a fresh one, and rebuild the probe.
            guard = None;
            if let Err(error) = signals.block(guest, None).await {
                break Err(error);
            }
            (call, guard) = call0.into_nonblocking(guest).await;
        }
        rsrc.poll_attempt += 1;
        if let Some((timeout, timeout_result)) = maybe_timeout {
            let new_time = thread_observe_time(guest).await;
            if new_time >= timeout {
                tracing::trace!(
                    "Timing out syscall after #{} retries: {}",
                    rsrc.poll_attempt - 1,
                    call.display(&guest.memory())
                );
                break timeout_result.map_err(|e| e.into());
            }
            tracing::trace!(
                "Retry #{} for syscall due to result {:?}, {} from timeout: {}",
                rsrc.poll_attempt,
                syscall_result,
                timeout - new_time,
                call.display(&guest.memory())
            );
        } else {
            tracing::trace!(
                "Retry #{} for syscall due to result {:?}: {}",
                rsrc.poll_attempt,
                syscall_result,
                call.display(&guest.memory())
            );
        }
        record_retry_event(guest, call).await;
    };

    drop(guard);
    let result = match result_after_restore(signals.restore(guest, None).await, result) {
        Ok(result) => result,
        Err(error) => return (Err(error), false),
    };
    // A signal the backend holds is delivered as the guest resumes, under the
    // guest's own mask (https://github.com/rrnewton/reverie/issues/1011).
    let deliver = deliver_under_wait_mask && !signals.holds_signal() && result.is_err();
    (
        refuse_held_signal_loss(guest, call0.number(), result).await,
        deliver,
    )
}

/// The policy of `Detcore::refuse_unserviceable_operation`, for a caller that
/// holds the guest but not the tool. `panic_on_unsupported_syscalls` is the
/// setting of that name: the tool's own copy in the method, the guest's
/// configuration in `refuse_held_signal_loss`.
pub(crate) async fn refuse_unserviceable_operation_with<T, G>(
    guest: &mut G,
    panic_on_unsupported_syscalls: bool,
    sysno: Sysno,
    fallback: Errno,
) -> Result<i64, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if !panic_on_unsupported_syscalls {
        return Err(fallback.into());
    }
    if guest.config().shutdown_on_unsupported_syscall {
        // Fail-closed policy: the operation is unserviceable and the config
        // forbids passing it through.
        crate::tool_global::unrecoverable_shutdown(
            guest,
            detcore_model::HERMIT_POLICY_REFUSAL_EXIT,
        )
        .await;
    }
    if guest.config().exit_on_unsupported_syscall {
        return Err(Error::Tool(anyhow::Error::new(
            crate::UnsupportedSyscallError(sysno),
        )));
    }
    panic!("unserviceable operation on syscall: {sysno:?}");
}

/// Refuse the guest's `sysno` call when its wait ended with a
/// [`HeldSignalRefusal`], through the unsupported-operation policy
/// (`refuse_unserviceable_operation_with`): a fail-closed run, which ordinary
/// `hermit run` is, ends with the policy's exit, and a run started with
/// `--allow-unsupported-syscalls` gets `ERESTARTNOINTR`, the transparent
/// restart that such a wait returned before (round-10 High 1 on
/// https://github.com/rrnewton/hermit/pull/3361). The message names the call,
/// the signals and that pull request. Callers pass the wait's result after the
/// wait has put the guest's mask back. Every other result passes through.
///
/// Whether a wait meets such a stop can depend on host timing, as a signal
/// sent from outside the guest does, so a refusal can be host-timed: it
/// replaces a run that would have gone on to depend on that timing.
pub(crate) async fn refuse_held_signal_loss<T, G>(
    guest: &mut G,
    sysno: Sysno,
    result: Result<i64, Error>,
) -> Result<i64, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    use std::io::Write as _;
    let refusal = match &result {
        Err(Error::Tool(error)) => match error.downcast_ref::<HeldSignalRefusal>() {
            Some(refusal) => HeldSignalRefusal {
                wait: Some(sysno),
                ..*refusal
            },
            None => return result,
        },
        _ => return result,
    };
    let panic_on_unsupported_syscalls = guest.config().panic_on_unsupported_syscalls;
    tracing::error!("[tid {}] {}", guest.tid(), refusal);
    if panic_on_unsupported_syscalls {
        // The run ends: report why even when logging is disabled or redirected,
        // as the refusal of a nonleader exec does.
        let _ = writeln!(crate::util::RetryingStderr, "{refusal}");
    }
    refuse_unserviceable_operation_with(
        guest,
        panic_on_unsupported_syscalls,
        sysno,
        Errno::ERESTARTNOINTR,
    )
    .await
}

/// Signal handling for one blocking wait whose interruption is decided from the
/// kernel's signal state (`backend_supports_blocked_wait_signal_interruption`).
///
/// A wait ends early only for a signal that would end it natively: one the guest
/// does not block and that is caught, or whose default action terminates or stops
/// the process (https://github.com/rrnewton/hermit/issues/3146). Blocked,
/// ignored, and default-ignored signals do not end it, and neither does a default
/// job-control stop when `defers_default_stops` is set
/// (`KernelSignalState::interrupting_wait`), apart from the stops that
/// `inject_absorbing` cannot absorb safely, which end it with a restart.
///
/// The first probe runs under the guest's own mask. If it would block, `block`
/// blocks every blockable signal for the rest of the wait, so later probes cannot
/// be stopped by one and every signal that arrives stays pending in the kernel,
/// where `/proc` reports it. `interrupted_with_state` classifies the pending set
/// against the guest's mask and dispositions each turn, and `restore` puts the
/// guest's mask back before the call returns, so a pending interrupting signal
/// is delivered as the call returns its restart errno.
///
/// Probes and the mask change go through `inject_absorbing`, never
/// `inject_with_retry`. A signal that stops the guest around an injection is
/// dequeued from the kernel and held by the backend in a single slot, so a blind
/// retry that is stopped again would replace it. `inject_absorbing` works out
/// from `/proc` which signal the backend holds and injects again only when that
/// signal would not end the wait natively. Before the mask is set, a signal
/// pending at the turn's `/proc` read that would end the wait is reported by
/// `interrupted_with_state`, and any other unblocked one stops the next
/// injection and is absorbed; a signal that arrives after the read can stop an
/// injection in a way that cannot be identified, and is held without ending the
/// wait, so the wait keeps its deadline and its checks, unless the backend held
/// a signal whose loss matters, which ends the wait with a
/// [`HeldSignalRefusal`] (`inject_absorbing`).
/// Afterwards only a signal that cannot be blocked can stop an injection. A stop
/// after a probe ran replaces its result, so a probe that consumed something (an
/// edge-triggered event, a dequeued signal) loses it; that remains a known gap.
///
/// No failure of this machinery reaches the guest as an errno the wait call
/// cannot return. A `/proc` read that fails for a thread that still exists, or a
/// guest mask that cannot be put back, ends the run with a
/// [`BlockedWaitSignalError`]; a thread that no longer exists gets
/// `ERESTARTNOINTR`, which nothing observes (`read_wait_signal_state`). If the
/// mask cannot be changed at all (no scratch room below the guest's stack
/// pointer, https://github.com/rrnewton/hermit/issues/3328), `block` leaves the
/// guest's own mask in place and every later probe runs under it, as the first
/// probe does.
/// The real-time signals, 32 to 64, as kernel sigset bits.
pub(crate) const REAL_TIME_SIGNALS: KernelSigset = !0 << 31;

/// The signals a thread whose mask becomes `mask` lets the kernel take from
/// its queues: those `mask` does not block, split into the ones whose delivery
/// the guest observes and the ones Linux discards on delivery because their
/// disposition is `SIG_IGN`.
pub(crate) fn unblocked_by(
    state: &KernelSignalState,
    mask: KernelSigset,
) -> (KernelSigset, KernelSigset) {
    let unblocked = !mask;
    (unblocked & !state.ignored, unblocked & state.ignored)
}

/// The real-time timer expirations among `candidates` (a kernel sigset) that
/// are pending for the calling thread's process because every thread blocked
/// them when they expired (`Scheduler::pending_real_time_expirations`,
/// https://github.com/rrnewton/hermit/issues/4089). 0 without asking the
/// scheduler until one has been kept pending in this run
/// (`scheduler::real_time_expiration_kept_pending`), or when `candidates`
/// holds no real-time signal. Asked in the calling thread's turn.
pub(crate) async fn pending_real_time_among<T, G>(guest: &mut G, candidates: KernelSigset) -> u64
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let candidates = candidates & REAL_TIME_SIGNALS;
    if candidates == 0 || !crate::scheduler::real_time_expiration_kept_pending() {
        return 0;
    }
    let Some(detpid) = guest.thread_state().detpid else {
        return 0;
    };
    crate::tool_global::pending_real_time_expirations(guest, detpid).await & candidates
}

/// Where a call lets the guest observe the signals in `observable` (deliver,
/// dequeue or report them), refuse the run by name if one of the `pending`
/// real-time timer expirations (`pending_real_time_among`) is among them, and
/// discard the pending ones in `discarded`, which Linux discards there.
pub(crate) async fn act_on_pending_real_time<T, G>(
    guest: &mut G,
    pending: KernelSigset,
    observable: KernelSigset,
    discarded: KernelSigset,
) where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let observed = pending & observable;
    if observed != 0 {
        crate::scheduler::refuse_real_time_expiration(observed.trailing_zeros() as i32 + 1);
    }
    if pending & discarded != 0
        && let Some(detpid) = guest.thread_state().detpid
    {
        crate::tool_global::discard_pending_real_time_expirations(
            guest,
            detpid,
            pending & discarded,
        )
        .await;
    }
}

/// For a call that runs under the temporary mask `mask`: refuse by name for a
/// pending real-time timer expiration the mask lets the kernel deliver, and
/// discard one it lets the kernel discard. Reads the thread's dispositions
/// only when one is pending.
pub(crate) async fn observe_under_mask<T, G>(guest: &mut G, mask: KernelSigset) -> Result<(), Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let pending = pending_real_time_among(guest, !mask).await;
    if pending != 0 {
        let state = read_thread_wait_signal_state(guest.pid(), guest.tid())?;
        let (observable, discarded) = unblocked_by(&state, mask);
        act_on_pending_real_time(guest, pending, observable, discarded).await;
    }
    Ok(())
}

pub(crate) struct KernelSignalWait {
    pid: reverie::Pid,
    tid: reverie::Pid,
    /// Signals the wait itself consumes (rt_sigtimedwait's set), which never
    /// interrupt it.
    consumed: KernelSigset,
    /// Whether a default job-control stop leaves the wait running, because Linux's
    /// restart of the call keeps its deadline
    /// (`NonblockableSyscall::restart_keeps_deadline`).
    defers_default_stops: bool,
    /// The errno that ends the wait for an interrupting signal: the restart code
    /// Linux uses for the call (`NonblockableSyscall::kernel_restart_errno`). A
    /// signal the backend holds turns `ERESTART_RESTARTBLOCK` into `ERESTARTNOHAND`
    /// (`held_signal_restart_errno`).
    restart_errno: Errno,
    /// Signals that never end the wait, even ones that would end it natively.
    /// They stay pending in the kernel, or held by the backend, and are
    /// delivered as the call returns, once the guest's mask is put back. In
    /// every wait but `select`'s and `pselect6`'s (`for_select`) this is
    /// `SIGCHLD`, whichever process sent it: the kernel also posts one for a
    /// child's exit, stop or continue at a moment set by host timing, and
    /// `/proc` shows no siginfo that would tell the two apart
    /// (https://github.com/rrnewton/hermit/issues/3146). The scheduler's own
    /// send of a `SIGCHLD` to this thread at a commit point lifts the hold for the
    /// turn whose resume names it (`admit_committed`,
    /// https://github.com/rrnewton/hermit/issues/4005). The gated loop adds,
    /// in each of its turns, every signal a host-timed source armed by a guest
    /// can post to this process (`hold_until_return`).
    held_until_return: KernelSigset,
    /// Whether the wait has a finite deadline (`with_deadline`). Hermit cannot
    /// install the restart block with which Linux keeps the absolute deadline
    /// of a restarted `poll` or timed futex wait, and a restarted `ppoll`,
    /// `epoll_wait`, `epoll_pwait` or `rt_sigtimedwait` runs again with the
    /// guest's unchanged timeout. So a transparent restart would start the
    /// timeout again, and such a wait never ends with `ERESTARTNOINTR`
    /// (`inject_absorbing`).
    deadline: bool,
    /// The guest's own mask while the wait runs with every signal blocked.
    saved_mask: Option<KernelSigset>,
    /// `block` could not change the mask, so probes run under the guest's own.
    unblockable: bool,
    /// The signal the backend holds since `inject_absorbing` absorbed a stop;
    /// `None` also after a stop it could not identify (`held_unidentified`).
    held: Option<HeldSignal>,
    /// The backend holds a signal from a stop that `inject_absorbing` could not
    /// identify, which may be one whose loss matters, so no later stop may
    /// replace it (`would_replace_precious`).
    held_unidentified: bool,
    /// The signals `hold_until_return` added because a host-timed source armed
    /// by a guest can post them; a committed signal never lifts these
    /// (`admit_committed`).
    host_timed: KernelSigset,
    /// Signals the scheduler committed to this thread in the current turn,
    /// which end the wait even though it holds them (`admit_committed`).
    committed: KernelSigset,
    /// The mask the wait sleeps under, when it is not the guest's own: the
    /// temporary mask of a `pselect6` (`with_wait_mask`). A signal decides
    /// against it whether it ends the wait; `restore` still puts back the
    /// guest's own mask (https://github.com/rrnewton/hermit/issues/3991).
    wait_mask: Option<KernelSigset>,
}

/// A signal that stopped one of a wait's injections and that the backend holds
/// for delivery when the guest resumes (`KernelSignalWait::inject_absorbing`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeldSignal {
    signal: i32,
    queue: SignalQueue,
    kind: HeldKind,
}

/// Whether losing a held signal, which a later stop would replace, changes what
/// the guest or the scheduler observes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum HeldKind {
    /// Its delivery does nothing: it is ignored, or default-ignored and not
    /// `SIGCHLD`.
    Harmless,
    /// Its delivery matters, or is kept as if it did: a signal the wait holds
    /// until the call returns (`held_until_return`), which may run a handler
    /// then, any other `SIGCHLD`, or a default job-control stop that the wait
    /// defers.
    Precious,
}

/// How a wait would lose a signal that the backend holds, or deliver it in
/// place of the result Linux gives (`HeldSignalRefusal`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeldSignalLoss {
    /// Another pending signal would stop the next injection, and the backend,
    /// which has one slot for a held signal, would drop the held one for it.
    Replaced,
    /// A stop that `/proc` cannot identify replaced a held signal whose loss
    /// matters (`HeldKind::Precious`), or one that could not be identified
    /// either and so may matter.
    UnidentifiedStop,
    /// The wait reached `MAX_ABSORBED_STOPS` absorbed stops while the backend
    /// held a signal whose loss matters or that could not be identified.
    StopBound,
    /// A signal that a wait without a deadline consumes stopped an injection,
    /// so the backend holds it and delivers it rather than the wait returning
    /// it, and either it is one the wait holds until the call returns
    /// (`held_until_return`) or it replaced a held signal whose loss matters.
    Consumed,
    /// A signal stopped an injection of `restore`, which puts the guest's mask
    /// back and so must run whatever is pending, and the backend dropped a held
    /// signal whose loss matters, or one that could not be identified, for it.
    Restoration,
}

/// A wait that ends because going on, or restarting the call, would lose a
/// signal the backend holds, or deliver it in place of the result Linux gives.
///
/// Every path that used to end such a wait with a fallback that returns
/// success (a transparent `ERESTARTNOINTR` restart, or a wait that went on and
/// let a stop replace the held signal) returns this instead (round-10 High 1 on
/// https://github.com/rrnewton/hermit/pull/3361). The wait puts the guest's mask
/// back, and its caller then applies the unsupported-operation policy
/// (`refuse_held_signal_loss`). A complete successful repair preserving signal
/// information needs additional framework support with the current generic
/// API: the backend holds one signal, does not say which, and delivers it
/// without the tool. A Detcore-only refusal or capability withdrawal can
/// prevent the determinism regression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HeldSignalRefusal {
    /// The guest call whose wait ends, once the caller names it
    /// (`refuse_held_signal_loss`).
    pub(crate) wait: Option<Sysno>,
    /// The signal the backend holds that would be lost or delivered wrongly;
    /// `None` when it could not be identified.
    pub(crate) held: Option<i32>,
    /// The signal that would replace it, or that stopped the injection, when
    /// known.
    pub(crate) replacing: Option<i32>,
    /// How it would be lost.
    pub(crate) loss: HeldSignalLoss,
}

/// A signal's name, such as `SIGCHLD`, or its number for a real-time signal.
fn signal_label(signal: Option<i32>) -> String {
    match signal {
        Some(signal) => match nix::sys::signal::Signal::try_from(signal) {
            Ok(named) => named.as_str().to_string(),
            Err(_) => format!("signal {signal}"),
        },
        None => "a signal that could not be identified".to_string(),
    }
}

impl std::fmt::Display for HeldSignalRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let wait = self
            .wait
            .map_or_else(|| "a blocking".to_string(), |sysno| sysno.to_string());
        let held = match self.held {
            Some(_) => format!("the held signal {}", signal_label(self.held)),
            None => "a held signal that could not be identified".to_string(),
        };
        let replacing = signal_label(self.replacing);
        match self.loss {
            HeldSignalLoss::Replaced => write!(
                f,
                "unsupported: the {wait} wait cannot keep {held}: the pending {replacing} \
                 would stop the next injection and replace it"
            )?,
            HeldSignalLoss::UnidentifiedStop => write!(
                f,
                "unsupported: the {wait} wait cannot keep {held}: a signal stop that /proc \
                 cannot identify replaced it"
            )?,
            HeldSignalLoss::StopBound => write!(
                f,
                "unsupported: the {wait} wait cannot keep {held}: the wait absorbed the \
                 maximum number of signal stops"
            )?,
            HeldSignalLoss::Consumed => {
                write!(
                    f,
                    "unsupported: the {wait} wait cannot return {replacing}, which it consumes: \
                     the signal stopped an injection, so the backend would deliver it instead"
                )?;
                if self.held.is_some() {
                    write!(f, ", in place of {held}")?;
                }
            }
            HeldSignalLoss::Restoration => write!(
                f,
                "unsupported: the {wait} wait cannot keep {held}: {replacing} stopped the \
                 injection that puts the guest's signal mask back and replaced it"
            )?,
        }
        write!(
            f,
            " (scope reduction of https://github.com/rrnewton/hermit/pull/3361; rerun with \
             --allow-unsupported-syscalls to restart the call instead)"
        )
    }
}

impl std::error::Error for HeldSignalRefusal {}

/// What a signal that stopped one of a wait's injections means for the wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopVerdict {
    /// The wait goes on; the backend holds the signal.
    Absorb(HeldKind),
    /// The wait ends with this errno.
    End(Errno),
}

/// The first real-time signal number in the kernel. A standard signal below it
/// is pending at most once; a second one sent while it is pending is merged.
const KERNEL_SIGRTMIN: i32 = 32;

impl KernelSignalWait {
    pub(crate) fn new<T, G>(
        guest: &G,
        consumed: KernelSigset,
        defers_default_stops: bool,
        restart_errno: Errno,
    ) -> Self
    where
        T: RecordOrReplay,
        G: Guest<Detcore<T>>,
    {
        Self {
            pid: guest.pid(),
            tid: guest.tid(),
            consumed,
            defers_default_stops,
            restart_errno,
            held_until_return: kernel_sigset_bit(libc::SIGCHLD),
            deadline: false,
            saved_mask: None,
            unblockable: false,
            held: None,
            held_unidentified: false,
            host_timed: 0,
            committed: 0,
            wait_mask: None,
        }
    }

    /// This wait, with a finite deadline when `deadline` is set (the `deadline`
    /// field): no fallback of `inject_absorbing` then ends it with
    /// `ERESTARTNOINTR`, which would start the guest's timeout again.
    pub(crate) fn with_deadline(self, deadline: bool) -> Self {
        Self { deadline, ..self }
    }

    /// The wait of `select` and of `pselect6`, which holds no signal until the
    /// call returns (`held_until_return` is empty); a `pselect6` with a
    /// temporary mask adds it with `with_wait_mask`:
    /// any pending signal that could interrupt it ends it, `SIGCHLD` included.
    ///
    /// Linux ends these calls with `ERESTARTNOHAND` whenever no descriptor is
    /// ready and a signal is pending (`core_sys_select`), so a caught `SIGCHLD`
    /// ends them whoever sent it. Detcore's select and pselect6 waits did the
    /// same before this machinery existed: their probes ran under the guest's
    /// own mask, and a pending caught signal stopped one.
    ///
    /// This is a known gap that these waits had before, and it is left as it
    /// was: the `SIGCHLD` the kernel posts for a child's exit, stop or continue
    /// ends the wait at the first turn whose read sees it, and host timing
    /// decides which turn that is (https://github.com/rrnewton/hermit/issues/3146).
    /// Every other wait holds `SIGCHLD` until it returns, unless the scheduler
    /// commits it to the thread in that turn (`admit_committed`).
    pub(crate) fn for_select<T, G>(guest: &G) -> Self
    where
        T: RecordOrReplay,
        G: Guest<Detcore<T>>,
    {
        Self {
            held_until_return: 0,
            ..Self::new(guest, 0, false, Errno::ERESTARTNOHAND)
        }
    }

    /// This wait, sleeping under `mask` instead of the guest's own (`wait_mask`):
    /// a `pselect6` with a temporary mask. Linux installs that mask for the
    /// whole call, so a signal it blocks never ends the call, even one the
    /// guest's own mask leaves unblocked, and one it leaves unblocked ends the
    /// call although the guest's own mask blocks it.
    pub(crate) fn with_wait_mask(self, mask: Option<KernelSigset>) -> Self {
        Self {
            wait_mask: mask,
            ..self
        }
    }

    /// Whether the backend holds a signal from a stop of one of this wait's
    /// injections (`held`, `held_unidentified`), which it delivers when the
    /// guest resumes.
    pub(crate) fn holds_signal(&self) -> bool {
        self.held.is_some() || self.held_unidentified
    }

    /// Whether a signal that ends the wait, pending in the kernel per `state`,
    /// is one the guest's own mask blocks: it ends the wait only because the
    /// wait mask (`with_wait_mask`) leaves it unblocked, and it stays pending
    /// when the guest's own mask is put back. A signal the wait holds until
    /// the call returns counts once the scheduler committed it in this turn
    /// (`admit_committed`), as in `interrupted_with_state`: otherwise a
    /// committed `SIGCHLD` that only the wait mask lets in ended the call
    /// without its handler (Claude review of
    /// https://github.com/rrnewton/hermit/pull/4217).
    pub(crate) fn interrupts_only_under_wait_mask(&self, state: &KernelSignalState) -> bool {
        let own_mask = self.saved_mask.unwrap_or(state.blocked);
        let held_until_return = self.held_until_return & !self.committed;
        self.wait_mask.is_some()
            && state.pending & self.could_interrupt(state) & !held_until_return & own_mask != 0
    }

    /// Adds `signals` to the ones this wait holds until the call returns
    /// (`held_until_return`). The gated loop passes, in each of its turns, the
    /// signals that a host-timed source armed by a guest can post to this
    /// process, such as a parent-death signal (`tool_global::host_timed_signals`):
    /// the kernel posts those at a moment set by host timing, so ending the wait
    /// for one would make the run depend on host timing. Never called for a
    /// `select` wait (`for_select`), which holds nothing.
    pub(crate) fn hold_until_return(&mut self, signals: KernelSigset) {
        self.held_until_return |= signals;
        self.host_timed |= signals;
    }

    /// Sets the signals the scheduler committed to this thread in this turn
    /// (`committed`): its resume named them (`ResumeStatus::Signaled`). Such a
    /// signal was sent at a fixed point of the schedule, so a wait may end for
    /// it although the wait holds it, unless a host-timed source can post it
    /// too (`host_timed`). The loop passes a committed `SIGCHLD`, which the
    /// scheduler sends at a commit point (a child's child-exit timer, or a
    /// POSIX timer whose signal is `SIGCHLD`); the kernel's
    /// own copy of it, posted at a host-timed moment, decides nothing, since
    /// only the resume that names it lifts the hold, for that turn alone
    /// (https://github.com/rrnewton/hermit/issues/4005).
    pub(crate) fn admit_committed(&mut self, signals: KernelSigset) {
        self.committed = signals & !self.host_timed;
    }

    /// Whether, of the signals that end the wait per `state`, only signals the
    /// scheduler committed in this turn do (`admit_committed`): without them
    /// the wait would go on.
    pub(crate) fn interrupts_only_by_committed(&self, state: &KernelSignalState) -> bool {
        let held = self.held.map_or(0, |held| kernel_sigset_bit(held.signal));
        let interrupting = (state.pending | held) & self.could_interrupt(state);
        self.committed != 0 && interrupting & !self.held_until_return == 0
    }

    /// Whether the caller should still call `block`: it has neither blocked the
    /// guest's signals nor found that it cannot.
    pub(crate) fn needs_block(&self) -> bool {
        self.saved_mask.is_none() && !self.unblockable
    }

    /// The signals that would end the wait natively under the guest's mask, given
    /// the kernel's `state`.
    /// The signals whose pending presence this wait would observe now: those
    /// it consumes, and those that would end it under the guest's mask, or
    /// the wait mask when it has one (`could_interrupt`). Read from the kernel's state in this turn.
    pub(crate) fn observable_now(&self) -> Result<KernelSigset, Error> {
        let state = read_wait_signal_state(self.pid, self.tid)?;
        Ok(self.consumed | self.could_interrupt(&state))
    }

    fn could_interrupt(&self, state: &KernelSignalState) -> KernelSigset {
        let guest_mask = self.wait_mask.or(self.saved_mask).unwrap_or(state.blocked);
        state.interrupting_wait(guest_mask, self.defers_default_stops) & !self.consumed
    }

    /// `interrupted_with_state` without the state, for the tests.
    #[cfg(test)]
    pub(crate) fn interrupted(&self) -> Result<bool, Error> {
        Ok(self.interrupted_with_state()?.0)
    }

    /// Whether a signal that would end the wait natively is pending, in which case
    /// the caller returns the call's restart errno, with the kernel's state that
    /// this read, which `inject_absorbing_after` takes as its first read in the same
    /// turn. The guest thread has been stopped inside the call since it was
    /// intercepted, so such a signal arrived while the call was waiting, and Linux
    /// returns the restart errno for that. Linux reports a source that was ready
    /// when the call began, or an argument error, before any pending signal, so
    /// `retry_blocking_wait_with_kernel_signal_state` runs its first probe before
    /// this check (`inject_first_probe`). There, a source that becomes ready in a
    /// later turn in which a signal is also pending is put behind the signal,
    /// because the turn cannot tell which came first. The waits of `select` and
    /// `pselect6` run every probe before this check, as Linux's do_select polls
    /// every descriptor on each pass before it tests for a pending signal. A
    /// signal the wait holds until the call returns (`held_until_return`) never
    /// counts. A signal that the backend holds since `inject_absorbing` absorbed
    /// it counts as pending. A failed read is never returned as the call's errno
    /// (`read_wait_signal_state`).
    pub(crate) fn interrupted_with_state(&self) -> Result<(bool, KernelSignalState), Error> {
        let state = read_wait_signal_state(self.pid, self.tid)?;
        let could_interrupt = self.could_interrupt(&state);
        let held = self.held.map_or(0, |held| kernel_sigset_bit(held.signal));
        let held_until_return = self.held_until_return & !self.committed;
        let interrupting = (state.pending | held) & could_interrupt & !held_until_return;
        if interrupting != 0 {
            tracing::trace!(
                "[tid {}] pending signals {:#x} interrupt a blocking wait",
                self.tid,
                interrupting
            );
        }
        Ok((interrupting != 0, state))
    }

    /// Inject `call`, a probe or a mask change, absorbing the signal stops that do
    /// not end the wait. `Ok` carries the call's own result; `Err` ends the wait.
    ///
    /// A signal that stops the guest around an injection is dequeued from the
    /// kernel and held by the backend, which delivers it when the guest resumes,
    /// and the injection returns a restart errno instead of the call's result.
    /// The backend does not say which signal it holds, so the kernel's state is
    /// read before each injection (in a polling turn of
    /// `retry_blocking_wait_with_kernel_signal_state` under the full mask, the
    /// turn's own read: `inject_absorbing_after`), which names the signal the
    /// kernel dequeues next (`KernelSignalState::next_dequeued`), and again
    /// after a stop. The stop is identified only if exactly that signal left its queue
    /// and the mask and dispositions did not change; it then decides:
    ///
    /// - A signal the wait consumes (`consumed`) is delivered rather than
    ///   consumed. In a wait without a deadline it ends the wait with
    ///   `ERESTARTNOINTR`, and the call runs again, unless the wait holds it
    ///   until the call returns (`held_until_return`), so that its delivery now
    ///   could follow host timing: then the wait ends with a
    ///   [`HeldSignalRefusal`]. In a wait with a deadline (`deadline`) it is
    ///   classified as if the wait did not consume it, by the rules below.
    /// - A signal that would end the wait natively ends it with the restart
    ///   errno, except a signal the wait holds until the call returns
    ///   (`held_until_return`), which is held.
    /// - Any other `SIGCHLD`, and a default job-control stop that the wait
    ///   defers, are held, and so are an ignored signal and a default-ignored
    ///   one. The injection runs again.
    /// - A signal that the wait mask (`with_wait_mask`) blocks is held: Linux
    ///   keeps it pending until the call returns.
    /// - Anything else, which is the backend's own preemption signal, ends the
    ///   wait with `ERESTARTNOINTR`, or is held in a wait with a deadline.
    ///
    /// A pending signal that would end the wait ends it before the injection,
    /// with the restart errno, so it is never held, except by a probe made by
    /// `inject_first_probe`: the first probe of a wait, and every probe of the
    /// waits of `select` and `pselect6`. A stop that cannot be identified, by a
    /// signal that arrived after the read, does not end the wait while the
    /// backend held nothing whose loss matters: the backend holds that signal
    /// in place of any held one and the injection runs again, as the blind
    /// retry before this machinery did, so the wait keeps its deadline and
    /// every check that ends it. Which signal it was would need the backend to
    /// say, so it is recorded only as unidentified (`held_unidentified`), and
    /// it is delivered as the call returns.
    ///
    /// The backend has one slot for a held signal, and a stop replaces what it
    /// holds. A held signal whose loss matters (`HeldKind::Precious`), or one
    /// that could not be identified and so may matter, is never given up for a
    /// result that reads as success, with or without a deadline:
    ///
    /// - An injection that another pending signal would stop is not made. If
    ///   the held signal would end the wait natively, the wait ends with the
    ///   restart errno, as Linux ends it for that signal; otherwise it ends
    ///   with a `HeldSignalRefusal`. The same standard signal on the same queue
    ///   is the exception, because Linux merges the two.
    /// - A stop that cannot be identified, which has already replaced such a
    ///   signal, ends the wait with a `HeldSignalRefusal`.
    /// - After `MAX_ABSORBED_STOPS` absorbed stops, a wait without a deadline
    ///   ends with a `HeldSignalRefusal`, or with `ERESTARTNOINTR` when the
    ///   backend holds nothing whose loss matters. A wait with a deadline,
    ///   whose restart would start the guest's timeout again, absorbs stops
    ///   without a bound.
    ///
    /// Each refusal is a scope reduction. It replaces a transparent
    /// `ERESTARTNOINTR` restart, or, in a wait with a deadline, an injection
    /// that went on and let a stop replace the held signal (round-10 High 1 on
    /// https://github.com/rrnewton/hermit/pull/3361): the restart made the
    /// backend deliver the held signal before the wait completed, at a moment
    /// host timing could set. A complete successful repair preserving signal
    /// information needs additional framework support with the current generic
    /// API. A Detcore-only refusal or capability withdrawal can prevent the
    /// determinism regression.
    ///
    /// In a probe made by `inject_first_probe` that two pending signals would
    /// stop in turn, the first of which would end the wait, the wait ends with
    /// the restart errno before the probe has run, so a ready source, or an
    /// argument error, is reported behind the signal: the order compromise that
    /// a later turn of `retry_blocking_wait_with_kernel_signal_state` already
    /// makes (`interrupted_with_state`).
    pub(crate) async fn inject_absorbing<T, G, S>(
        &mut self,
        guest: &mut G,
        call: S,
    ) -> Result<Result<i64, Errno>, Error>
    where
        T: RecordOrReplay,
        G: Guest<Detcore<T>>,
        S: SyscallInfo,
    {
        self.inject_absorbing_from(guest, call, None, false).await
    }

    /// The first probe of a wait, and every probe of the waits of `select` and
    /// `pselect6`, which runs before the turn's `interrupted_with_state` check:
    /// `inject_absorbing`, except that a signal that would end the wait neither
    /// ends it before the injection nor when it stops the injection.
    ///
    /// Linux reports a descriptor that is ready when the call begins, a queued
    /// event, and an argument error before it looks at pending signals:
    /// `do_poll` tests `signal_pending` only when no descriptor is ready, and so
    /// does `core_sys_select`, whose `max_select_fd` first returns `EBADF` for a
    /// descriptor in the sets that is not open; `ep_poll` sends queued events
    /// before it tests it, `do_epoll_wait` rejects a bad descriptor or
    /// `maxevents` first, and `do_sigtimedwait` copies its set first. So the
    /// probe runs even when such a signal is pending. `do_select` also polls
    /// every descriptor on each pass before it tests `signal_pending`, so the
    /// waits of `select` and `pselect6` make every probe this way; once `block`
    /// has blocked every blockable signal, a pending one cannot stop a probe
    /// there, and the check that follows a probe that would block counts it.
    /// A pending signal that the guest does not block stops the injection
    /// before the probe runs; when the stop is identified, the signal is held
    /// as a precious one (`HeldKind::Precious`) and the probe runs again, so
    /// its result stands, and the backend delivers the signal as the call
    /// returns. If the probe would block, the caller's check that follows
    /// counts the held signal (`interrupted_with_state`) and ends the wait with
    /// the restart errno. A stop that cannot be identified is held, as
    /// `inject_absorbing` describes.
    pub(crate) async fn inject_first_probe<T, G, S>(
        &mut self,
        guest: &mut G,
        call: S,
    ) -> Result<Result<i64, Errno>, Error>
    where
        T: RecordOrReplay,
        G: Guest<Detcore<T>>,
        S: SyscallInfo,
    {
        self.inject_absorbing_from(guest, call, None, true).await
    }

    /// `inject_absorbing` in a turn whose `interrupted_with_state` read `state`
    /// and found no signal that ends the wait.
    ///
    /// Once `block` has blocked every blockable signal (`saved_mask`), `state`
    /// serves as the first read before the injection, so a polling turn reads
    /// `/proc` once, as it did before injections absorbed stops. The caller must
    /// run nothing between the two that resumes the guest thread or changes its
    /// signal state. `retry_blocking_wait_with_kernel_signal_state`, which serves
    /// ppoll, poll, epoll_pwait, epoll_wait, rt_sigtimedwait and polling futex
    /// waits and is the only caller, runs nothing there. So between the two reads
    /// the guest thread stays stopped and only it can change its own mask, and
    /// with threads sequentialized no other guest thread runs in its turn, so a
    /// signal that arrives meanwhile arrives at a
    /// host-timed moment: sent from outside the guest, or posted by the kernel.
    /// A signal that the full mask blocks stays pending, cannot stop the
    /// injection, and is classified at the next turn's read, as one that arrives
    /// just after a fresh read is. One that the full mask leaves unblocked
    /// (`SIGKILL`, `SIGSTOP`, glibc's two reserved signals, `PERF_EVENT_SIGNAL`)
    /// and that stops the injection is not named by `state`, so the stop is not
    /// identified and is handled as a stop by a signal that arrives just after a
    /// fresh read is. `interrupted_with_state` found
    /// no signal in `state` that ends the wait, so the check before the injection
    /// does not end it either.
    ///
    /// Before `block` takes effect, or when it cannot, the probe runs under the
    /// guest's own mask and a fresh read is taken, so a signal that arrived
    /// after `state` is still identified or ends the wait before the injection.
    pub(crate) async fn inject_absorbing_after<T, G, S>(
        &mut self,
        guest: &mut G,
        call: S,
        state: KernelSignalState,
    ) -> Result<Result<i64, Errno>, Error>
    where
        T: RecordOrReplay,
        G: Guest<Detcore<T>>,
        S: SyscallInfo,
    {
        let first = self.saved_mask.is_some().then_some(state);
        self.inject_absorbing_from(guest, call, first, false).await
    }

    /// `inject_absorbing`, taking `first`, if any, as the state read before the
    /// first injection. For `first_probe`, a signal that would end the wait does
    /// not end it, and one that stops the injection is held
    /// (`inject_first_probe`).
    async fn inject_absorbing_from<T, G, S>(
        &mut self,
        guest: &mut G,
        call: S,
        mut first: Option<KernelSignalState>,
        first_probe: bool,
    ) -> Result<Result<i64, Errno>, Error>
    where
        T: RecordOrReplay,
        G: Guest<Detcore<T>>,
        S: SyscallInfo,
    {
        // Each absorbed stop takes one signal off a kernel queue, so only signals
        // sent faster than the injections run can reach this. A wait with a
        // deadline has no bound, as the blind retry before this machinery had
        // none: ending it would start its timeout again (`deadline`).
        const MAX_ABSORBED_STOPS: usize = 64;
        let mut absorbed = 0;
        loop {
            let before = match first.take() {
                Some(state) => state,
                None => read_wait_signal_state(self.pid, self.tid)?,
            };
            let ends_wait = self.could_interrupt(&before) & !self.held_until_return;
            if !first_probe && before.pending & ends_wait != 0 {
                return Err(self.restart_errno.into());
            }
            let next = before.next_dequeued(before.blocked);
            if let Some((signal, queue)) = next
                && self.would_replace_precious(signal, queue)
            {
                if let Some(held) = self.held
                    && self.held_ends_wait(&before, held.signal)
                {
                    tracing::trace!(
                        "[tid {}] signal {} would replace the held signal {}, which ends the wait",
                        self.tid,
                        signal,
                        held.signal
                    );
                    return Err(held_signal_restart_errno(self.restart_errno, held.signal).into());
                }
                // Round-10 High 1 on https://github.com/rrnewton/hermit/pull/3361:
                // this used to end a wait without a deadline with ERESTARTNOINTR,
                // and let a wait with one go on and lose the held signal.
                tracing::trace!(
                    "[tid {}] signal {} would replace the held signal {:?}; the wait is refused",
                    self.tid,
                    signal,
                    self.held
                );
                return Err(self.refusal(Some(signal), HeldSignalLoss::Replaced));
            }
            let result = guest.inject(call).await;
            let Err(errno) = result else {
                return Ok(result);
            };
            if !probe_was_interrupted_by_signal(errno) {
                return Ok(result);
            }
            let after = read_wait_signal_state(self.pid, self.tid)?;
            // A probe that carries the call's temporary mask, as a masked
            // ppoll's does (`with_wait_mask`), and that the kernel itself
            // ended, for a pending signal the mask lets in, leaves that mask
            // installed with its restore pending until the signal is delivered
            // as the guest resumes, so `/proc` reports a mask other than the one
            // before the injection. That ERESTARTNOHAND is the call's own
            // result, not a stop, and no injection may follow it: a repeated
            // probe would save the temporary mask as the guest's
            // (https://github.com/rrnewton/reverie/issues/1003). A pselect6
            // wait's probes carry no mask, so this never holds for one.
            if self.wait_mask.is_some()
                && errno == Errno::ERESTARTNOHAND
                && after.blocked != before.blocked
            {
                return Ok(result);
            }
            let identified =
                next.filter(|&(signal, queue)| stop_took_only(&before, &after, signal, queue));
            let Some((signal, queue)) = identified else {
                // A signal that `before` did not show stopped the injection. The
                // backend holds it in place of any held one and delivers it when
                // the guest resumes, but `/proc` cannot name it. If the backend
                // held a signal whose loss matters, or one it could not identify
                // either, that signal is gone, and the wait is refused. Otherwise
                // the wait goes on and the injection runs again, as the blind
                // retry before this machinery did, so the wait keeps its
                // deadline; the signals that `/proc` still shows are classified
                // as before, and every check that ends the wait still runs at
                // the read before the injection and at each turn's
                // `interrupted_with_state`.
                if self.holds_precious() {
                    tracing::trace!(
                        "[tid {}] a signal stop that cannot be identified replaced the held signal {:?}; the wait is refused",
                        self.tid,
                        self.held
                    );
                    return Err(self.refusal(None, HeldSignalLoss::UnidentifiedStop));
                }
                if !self.deadline && absorbed >= MAX_ABSORBED_STOPS {
                    // The backend now holds the unidentified signal.
                    self.held = None;
                    self.held_unidentified = true;
                    return Err(self.refusal(None, HeldSignalLoss::StopBound));
                }
                absorbed += 1;
                tracing::trace!(
                    "[tid {}] a signal stop that cannot be identified is held; the injection runs again",
                    self.tid
                );
                self.held = None;
                self.held_unidentified = true;
                continue;
            };
            let bit = kernel_sigset_bit(signal);
            let verdict = match self.stop_verdict(&before, signal) {
                // A probe made by `inject_first_probe` holds a signal that would
                // end the wait and runs again.
                StopVerdict::End(_)
                    if first_probe
                        && self.consumed & bit == 0
                        && self.could_interrupt(&before) & bit != 0 =>
                {
                    StopVerdict::Absorb(HeldKind::Precious)
                }
                verdict => verdict,
            };
            match verdict {
                StopVerdict::Absorb(kind) => {
                    let kind = self.held.map_or(kind, |held| held.kind.max(kind));
                    let unidentified = self.held_unidentified;
                    self.held = Some(HeldSignal {
                        signal,
                        queue,
                        kind,
                    });
                    self.held_unidentified = false;
                    if !self.deadline && absorbed >= MAX_ABSORBED_STOPS {
                        if kind == HeldKind::Precious || unidentified {
                            return Err(self.refusal(None, HeldSignalLoss::StopBound));
                        }
                        return Err(Errno::ERESTARTNOINTR.into());
                    }
                    absorbed += 1;
                    tracing::trace!(
                        "[tid {}] signal {} stopped an injection and is held ({:?})",
                        self.tid,
                        signal,
                        kind
                    );
                }
                // A signal that a wait without a deadline consumes, delivered
                // rather than returned: refused when the wait holds it until the
                // call returns, or when it replaced a held signal whose loss
                // matters, and otherwise ended with ERESTARTNOINTR (stop_verdict).
                StopVerdict::End(_)
                    if !self.deadline
                        && self.consumed & bit != 0
                        && (self.held_until_return & bit != 0 || self.holds_precious()) =>
                {
                    tracing::trace!(
                        "[tid {}] consumed signal {} stopped an injection; the wait is refused",
                        self.tid,
                        signal
                    );
                    return Err(self.refusal(Some(signal), HeldSignalLoss::Consumed));
                }
                // The backend holds `signal`, which stopped the injection.
                StopVerdict::End(errno) => {
                    return Err(held_signal_restart_errno(errno, signal).into());
                }
            }
        }
    }

    /// What a stop by `signal` means for the wait, given the kernel's state
    /// `before` the injection (see `inject_absorbing`).
    fn stop_verdict(&self, before: &KernelSignalState, signal: i32) -> StopVerdict {
        let bit = kernel_sigset_bit(signal);
        let interrupts = if self.consumed & bit != 0 {
            if !self.deadline {
                return StopVerdict::End(Errno::ERESTARTNOINTR);
            }
            // The backend delivers the signal rather than the wait consuming it,
            // as Linux does for one the guest does not block, so it is classified
            // as if the wait did not consume it.
            let guest_mask = self.wait_mask.or(self.saved_mask).unwrap_or(before.blocked);
            before.interrupting_wait(guest_mask, self.defers_default_stops) & bit != 0
        } else {
            self.could_interrupt(before) & bit != 0
        };
        if interrupts {
            return if self.held_until_return & bit != 0 {
                StopVerdict::Absorb(HeldKind::Precious)
            } else {
                StopVerdict::End(self.restart_errno)
            };
        }
        if signal == libc::SIGCHLD
            || (self.defers_default_stops && before.default_job_control_stops() & bit != 0)
        {
            return StopVerdict::Absorb(HeldKind::Precious);
        }
        let default_ignored = [libc::SIGCONT, libc::SIGURG, libc::SIGWINCH]
            .into_iter()
            .fold(0, |set, signal| set | kernel_sigset_bit(signal));
        if (before.ignored | (default_ignored & !before.caught)) & bit != 0 {
            return StopVerdict::Absorb(HeldKind::Harmless);
        }
        // A signal that the wait mask blocks, and the guest's own mask does
        // not, can stop an injection that runs before `block` takes effect.
        // Linux keeps it pending for the whole call, under the temporary mask,
        // and delivers it as the call returns, so it is held and never ends
        // the wait (review of
        // https://github.com/rrnewton/hermit/pull/4051).
        if self.deadline || self.wait_mask.is_some_and(|mask| mask & bit != 0) {
            StopVerdict::Absorb(HeldKind::Precious)
        } else {
            StopVerdict::End(Errno::ERESTARTNOINTR)
        }
    }

    /// Whether an injection that `signal`, pending on `queue`, stops would make
    /// the backend drop a held signal whose delivery matters
    /// (`HeldKind::Precious`), or one it could not identify, which may matter
    /// (`held_unidentified`). Linux merges a standard signal into the same one
    /// pending on the same queue, so that one replaces nothing.
    fn would_replace_precious(&self, signal: i32, queue: SignalQueue) -> bool {
        self.held_unidentified
            || self.held.is_some_and(|held| {
                held.kind == HeldKind::Precious
                    && !(signal == held.signal && queue == held.queue && signal < KERNEL_SIGRTMIN)
            })
    }

    /// Whether the backend holds a signal whose loss matters
    /// (`HeldKind::Precious`), or one it could not identify, which may matter.
    fn holds_precious(&self) -> bool {
        self.held_unidentified
            || self
                .held
                .is_some_and(|held| held.kind == HeldKind::Precious)
    }

    /// The `HeldSignalRefusal` that ends the wait, naming the signal the backend
    /// holds, if known, and `replacing`, the one that would replace it or that
    /// stopped the injection.
    fn refusal(&self, replacing: Option<i32>, loss: HeldSignalLoss) -> Error {
        Error::Tool(anyhow::Error::new(HeldSignalRefusal {
            wait: None,
            held: self.held.map(|held| held.signal),
            replacing,
            loss,
        }))
    }

    /// Whether a held `signal` would end the wait natively, given the kernel's
    /// state `before` an injection: it could interrupt the wait, and the wait
    /// does not hold it until the call returns (`held_until_return`).
    fn held_ends_wait(&self, before: &KernelSignalState, signal: i32) -> bool {
        self.could_interrupt(before) & !self.held_until_return & kernel_sigset_bit(signal) != 0
    }

    /// Block every blockable signal for the rest of the wait, and keep blocked the
    /// signals the guest blocks itself, which the blockable set leaves out (the C
    /// library's reserved signals). `scratch` is a guest cell for the mask when
    /// the caller holds the only scratch-stack guard; otherwise a fresh guard is
    /// taken. The change goes through `inject_absorbing`, so a signal that stops
    /// it is handled as one that stops a probe. If that ends the wait, the guest's
    /// mask is still recorded, and the caller's final `restore` reads the
    /// kernel's mask and puts the guest's back if the change took effect.
    ///
    /// If the mask cannot be changed for another reason, the guest's mask is
    /// unchanged and no wait call can return that error, so the wait continues
    /// with every probe under the guest's own mask, as the first probe runs, and
    /// `block` is not tried again (`needs_block`). `interrupted_with_state` reads
    /// that mask from the kernel each turn. The scratch-stack commit fails this
    /// way on a guest stack with no room below its stack pointer
    /// (https://github.com/rrnewton/hermit/issues/3328); poll and epoll_wait
    /// probes need no scratch of their own, so for them this is the first need.
    pub(crate) async fn block<'a, T, G>(
        &mut self,
        guest: &mut G,
        scratch: Option<AddrMut<'a, KernelSigset>>,
    ) -> Result<(), Error>
    where
        T: RecordOrReplay,
        G: Guest<Detcore<T>>,
    {
        let guest_mask = read_wait_signal_state(self.pid, self.tid)?.blocked;
        let mask = blocked_signal_mask() | guest_mask;
        let result = match scratch {
            Some(cell) => match guest.memory().write_value(cell, &mask) {
                Ok(()) => {
                    self.inject_absorbing(guest, set_signal_mask_call(cell.into()))
                        .await
                }
                Err(errno) => Ok(Err(errno)),
            },
            None => {
                let mut stack = guest.stack().await;
                let cell = stack.push(mask);
                match stack.commit() {
                    Ok(_guard) => {
                        self.inject_absorbing(guest, set_signal_mask_call(cell))
                            .await
                    }
                    Err(_) => Ok(Err(Errno::EFAULT)),
                }
            }
        };
        match result {
            Ok(Ok(_)) => {
                self.saved_mask = Some(guest_mask);
                Ok(())
            }
            Ok(Err(errno)) => {
                tracing::debug!(
                    "[tid {}] cannot block signals for a wait ({}); its probes run under the \
                     guest's mask",
                    self.tid,
                    errno
                );
                self.unblockable = true;
                Ok(())
            }
            Err(error) => {
                self.saved_mask = Some(guest_mask);
                Err(error)
            }
        }
    }

    /// Put back the guest's mask, if `block` replaced it. A signal can stop the guest
    /// around the call, before or after it takes effect, so success is read back
    /// from the kernel and the call is repeated until the kernel reports the
    /// guest's mask. The guest never resumes with every signal blocked: if the
    /// mask cannot be put back, the run ends with
    /// [`BlockedWaitSignalError::MaskNotRestored`].
    ///
    /// The backend holds the signal that stops one of these injections, in place
    /// of any it held, as it does for a probe (`inject_absorbing`). Each such
    /// stop is recorded from the read before the injection and the read after it
    /// (`absorb_restoration_stop`). If one replaced a held signal whose loss
    /// matters, or one that could not be identified, the mask is still put back
    /// and then the first such loss is returned as a [`HeldSignalRefusal`]
    /// (`HeldSignalLoss::Restoration`), which the caller turns into the wait's
    /// result (`result_after_restore`). Round-11 High F1 on
    /// https://github.com/rrnewton/hermit/pull/3361: these stops used to be
    /// retried without that check, so the held signal was lost silently.
    pub(crate) async fn restore<'a, T, G>(
        &mut self,
        guest: &mut G,
        scratch: Option<AddrMut<'a, KernelSigset>>,
    ) -> Result<(), Error>
    where
        T: RecordOrReplay,
        G: Guest<Detcore<T>>,
    {
        let Some(guest_mask) = self.saved_mask else {
            return Ok(());
        };
        // `stop_verdict` reads the guest's mask from `saved_mask`, so it is cleared
        // only once the restoration has ended.
        let restored = self.restore_mask(guest, guest_mask, scratch).await;
        self.saved_mask = None;
        restored
    }

    /// `restore`, for a wait whose guest mask is `guest_mask`.
    async fn restore_mask<'a, T, G>(
        &mut self,
        guest: &mut G,
        guest_mask: KernelSigset,
        scratch: Option<AddrMut<'a, KernelSigset>>,
    ) -> Result<(), Error>
    where
        T: RecordOrReplay,
        G: Guest<Detcore<T>>,
    {
        // Under the all-blocked mask only a signal that cannot be blocked (SIGKILL,
        // SIGSTOP) can stop the call before it takes effect, and each such stop needs
        // another signal sent, so a bound this large is never reached in practice.
        const ATTEMPTS: usize = 16;
        let mut attempts = 0;
        let mut last_error = None;
        // The kernel's state before an injection that a signal stopped, until the
        // next read records the stop.
        let mut stopped = None;
        // The first held signal that a stop replaced, as the refusal to return once
        // the mask is back.
        let mut loss = None;
        while attempts < ATTEMPTS {
            let state = read_wait_signal_state(self.pid, self.tid)?;
            if let Some(before) = stopped.take() {
                let replaced = self.absorb_restoration_stop(&before, &state);
                loss = loss.or(replaced);
            }
            if state.blocked == guest_mask {
                return loss.map_or(Ok(()), Err);
            }
            attempts += 1;
            match inject_signal_mask(guest, guest_mask, scratch).await {
                Ok(()) => return loss.map_or(Ok(()), Err),
                // Stopped by a signal: the next read records the stop and decides
                // whether the mask took effect.
                Err(errno) if probe_was_interrupted_by_signal(errno) => {
                    last_error = Some(errno);
                    stopped = Some(state);
                }
                // The call could not run, so the mask is unchanged.
                Err(errno) => {
                    last_error = Some(errno);
                    break;
                }
            }
        }
        let state = read_wait_signal_state(self.pid, self.tid)?;
        if let Some(before) = stopped.take() {
            let replaced = self.absorb_restoration_stop(&before, &state);
            loss = loss.or(replaced);
        }
        if state.blocked == guest_mask {
            return loss.map_or(Ok(()), Err);
        }
        Err(Error::Tool(anyhow::Error::new(
            BlockedWaitSignalError::MaskNotRestored {
                pid: self.pid,
                tid: self.tid,
                attempts,
                last_error,
            },
        )))
    }

    /// Record a signal stop of an injection of `restore`, given the kernel's state
    /// `before` the injection and `after` it, as `inject_absorbing` records a stop
    /// of a probe: the backend holds the signal that stopped it in place of any it
    /// held. Returns the `HeldSignalRefusal` when that replaced a held signal
    /// whose loss matters, or one that could not be identified
    /// (`would_replace_precious`, `holds_precious`), naming the held signal as it
    /// was before the stop.
    ///
    /// A stop is identified only when exactly the signal that the kernel would
    /// take next under the mask `before` left its queue and the mask did not
    /// change (`stop_took_only`); a stop after which the mask took effect, or one
    /// that `before` did not show, is recorded as unidentified. The call's result
    /// is already decided, so a signal that would have ended the wait is kept as
    /// one whose delivery matters.
    fn absorb_restoration_stop(
        &mut self,
        before: &KernelSignalState,
        after: &KernelSignalState,
    ) -> Option<Error> {
        let identified = before
            .next_dequeued(before.blocked)
            .filter(|&(signal, queue)| stop_took_only(before, after, signal, queue));
        let Some((signal, queue)) = identified else {
            let loss = self
                .holds_precious()
                .then(|| self.refusal(None, HeldSignalLoss::Restoration));
            tracing::trace!(
                "[tid {}] a signal stop that cannot be identified stopped the mask restoration; it replaced the held signal {:?}",
                self.tid,
                self.held
            );
            self.held = None;
            self.held_unidentified = true;
            return loss;
        };
        let loss = self
            .would_replace_precious(signal, queue)
            .then(|| self.refusal(Some(signal), HeldSignalLoss::Restoration));
        let kind = match self.stop_verdict(before, signal) {
            StopVerdict::Absorb(kind) => kind,
            StopVerdict::End(_) => HeldKind::Precious,
        };
        let kind = self.held.map_or(kind, |held| held.kind.max(kind));
        tracing::trace!(
            "[tid {}] signal {} stopped the mask restoration and is held ({:?}) in place of {:?}",
            self.tid,
            signal,
            kind,
            self.held
        );
        self.held = Some(HeldSignal {
            signal,
            queue,
            kind,
        });
        self.held_unidentified = false;
        loss
    }
}

/// The wait's result once `restore` has put the guest's mask back. A
/// [`HeldSignalRefusal`] from the restoration (`HeldSignalLoss::Restoration`)
/// replaces `result`, as a loss during the wait does, unless `result` is
/// already such a refusal, which came first; the caller then applies the
/// unsupported-operation policy (`refuse_held_signal_loss`). Every other
/// restoration error, such as [`BlockedWaitSignalError::MaskNotRestored`], is
/// returned as the error that ends the run, as before.
pub(crate) fn result_after_restore(
    restored: Result<(), Error>,
    result: Result<i64, Error>,
) -> Result<Result<i64, Error>, Error> {
    let is_refusal =
        |error: &Error| matches!(error, Error::Tool(error) if error.is::<HeldSignalRefusal>());
    match restored {
        Ok(()) => Ok(result),
        Err(refusal) if is_refusal(&refusal) => match &result {
            Err(error) if is_refusal(error) => Ok(result),
            _ => Ok(Err(refusal)),
        },
        Err(error) => Err(error),
    }
}

/// Replace the guest's signal mask with `mask`, written to `scratch` or, when the
/// caller holds no scratch-stack guard, to a fresh one.
async fn inject_signal_mask<'a, T, G>(
    guest: &mut G,
    mask: KernelSigset,
    scratch: Option<AddrMut<'a, KernelSigset>>,
) -> Result<(), Errno>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    match scratch {
        Some(cell) => {
            guest.memory().write_value(cell, &mask)?;
            guest
                .inject(set_signal_mask_call(cell.into()))
                .await
                .map(drop)
        }
        None => {
            let mut stack = guest.stack().await;
            let cell = stack.push(mask);
            let _guard = stack.commit().map_err(|_| Errno::EFAULT)?;
            guest.inject(set_signal_mask_call(cell)).await.map(drop)
        }
    }
}

/// `rt_sigprocmask(SIG_SETMASK, cell, NULL)`.
fn set_signal_mask_call(cell: Addr<'_, KernelSigset>) -> syscalls::RtSigprocmask {
    syscalls::RtSigprocmask::new()
        .with_how(libc::SIG_SETMASK)
        .with_set(Some(cell.cast()))
        .with_oldset(None)
        .with_sigsetsize(KERNEL_SIGSET_SIZE)
}

/// Whether a signal stop between the kernel states `before` and `after` took
/// exactly `signal` off `queue`: it is no longer there, no other signal left
/// either queue, and the mask and dispositions did not change. Signals may have
/// arrived in between.
fn stop_took_only(
    before: &KernelSignalState,
    after: &KernelSignalState,
    signal: i32,
    queue: SignalQueue,
) -> bool {
    let bit = kernel_sigset_bit(signal);
    let other = match queue {
        SignalQueue::Thread => SignalQueue::Shared,
        SignalQueue::Shared => SignalQueue::Thread,
    };
    before.blocked == after.blocked
        && before.ignored == after.ignored
        && before.caught == after.caught
        && after.queued(queue) & bit == 0
        && before.queued(queue) & !bit & !after.queued(queue) == 0
        && before.queued(other) & !after.queued(other) == 0
}

/// Whether an injected call's error means a signal stopped the guest around it
/// rather than that the call produced a result: the backend's restart errno, or
/// `EINTR` where a backend runs the call itself. None of these calls blocks.
pub(crate) fn probe_was_interrupted_by_signal(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::EINTR
            | Errno::ERESTARTSYS
            | Errno::ERESTARTNOINTR
            | Errno::ERESTARTNOHAND
            | Errno::ERESTART_RESTARTBLOCK
    )
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
    match positive_millis_as_nanos(timeout_millis) {
        Some(timeout_nanos) => nanos_duration_to_absolute_timeout(guest, timeout_nanos).await,
        None => None,
    }
}

/// Milliseconds to nanoseconds for a strictly positive timeout; `None` for the
/// non-positive values Linux treats as "return immediately" (0) or "wait
/// forever" (-1), neither of which is a deadline.
///
/// Kept as a separate, unit-bracketed function on purpose. This conversion was
/// previously inlined as `* 1000` instead of `* 1_000_000`, which made every
/// finite deadline 1000x too short (a 1 ms timeout expired after 1 us). That
/// is invisible in an end-to-end test that only checks a syscall's return
/// value, so the arithmetic is pinned here by `millis_to_nanos_conversion`.
fn positive_millis_as_nanos(timeout_millis: i32) -> Option<u128> {
    (timeout_millis > 0).then(|| (timeout_millis as u128) * 1_000_000)
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

/// The number of `pollfd` entries `poll` and `ppoll` read. Linux declares
/// their `nfds` parameter `unsigned int`, so only the low 32 bits of the
/// register count; a guest may leave the high word set. Every reader of a
/// guest's pollfd array sizes it from this, never from `nfds` itself.
pub fn poll_nfds(nfds: libc::nfds_t) -> u32 {
    nfds as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bracket the millisecond-to-nanosecond timeout conversion in both
    /// directions: the non-positive values that are NOT deadlines, and the
    /// positive values whose magnitude must be exactly 1e6 ns per ms.
    ///
    /// The 1 ms case is the specific regression guard: an inlined `* 1000`
    /// yields 1_000 here instead of 1_000_000, i.e. a deadline 1000x too
    /// short. Asserting the exact value (not merely "nonzero" or "greater
    /// than") is what makes that failure visible.
    #[test]
    fn millis_to_nanos_conversion() {
        // Not deadlines: -1 is "infinite", 0 is "return immediately".
        assert_eq!(positive_millis_as_nanos(-1), None);
        assert_eq!(positive_millis_as_nanos(i32::MIN), None);
        assert_eq!(positive_millis_as_nanos(0), None);

        // Positive timeouts: exactly 1e6 nanoseconds per millisecond.
        assert_eq!(positive_millis_as_nanos(1), Some(1_000_000));
        assert_eq!(positive_millis_as_nanos(1_000), Some(1_000_000_000));
        assert_eq!(
            positive_millis_as_nanos(i32::MAX),
            Some(i32::MAX as u128 * 1_000_000)
        );

        // The scale itself, stated independently of any single case so a
        // future refactor cannot satisfy the above by coincidence.
        for millis in [1, 2, 7, 250, 1_000, 86_400_000] {
            assert_eq!(
                positive_millis_as_nanos(millis),
                Some(millis as u128 * 1_000_000),
                "1 ms must convert to 1_000_000 ns, not 1_000"
            );
        }
    }

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
        // A zero-progress writev interruption returns this internal errno on
        // ptrace so Linux applies the handler's SA_RESTART policy.
        assert_eq!(
            reverie::syscalls::Writev::new().signal_interrupt_errno(),
            Errno::ERESTARTSYS
        );
        assert_eq!(
            reverie::syscalls::Futex::new().signal_interrupt_errno(),
            Errno::ERESTARTSYS
        );
    }

    #[test]
    fn kernel_restart_errno_matches_linux_restart_policy() {
        // Signal-state interruption hands the kernel the wait's own restart code, so
        // the guest's disposition decides between a handler's EINTR and a restart
        // (https://github.com/rrnewton/hermit/issues/3146).
        // A poll with a positive timeout and a timed futex wait end with Linux's own
        // code: a handler turns it into EINTR, and with no handler the kernel runs
        // restart_syscall, which Detcore resumes with the deadline it kept
        // (`RestartBlock`, https://github.com/rrnewton/hermit/issues/3358).
        assert_eq!(
            reverie::syscalls::Poll::new()
                .with_timeout(300)
                .kernel_restart_errno(),
            Errno::ERESTART_RESTARTBLOCK
        );
        // A zero timeout never blocks and a negative one never expires, so there is
        // no deadline to keep, and running the call again is Linux's restart.
        assert_eq!(
            reverie::syscalls::Poll::new().kernel_restart_errno(),
            Errno::ERESTARTNOHAND
        );
        assert_eq!(
            reverie::syscalls::Poll::new()
                .with_timeout(-1)
                .kernel_restart_errno(),
            Errno::ERESTARTNOHAND
        );
        assert_eq!(
            reverie::syscalls::Ppoll::new().kernel_restart_errno(),
            Errno::ERESTARTNOHAND
        );
        // epoll_wait and sigtimedwait never restart after a handler.
        assert_eq!(
            reverie::syscalls::EpollWait::new().kernel_restart_errno(),
            Errno::EINTR
        );
        assert_eq!(
            reverie::syscalls::RtSigtimedwait::new().kernel_restart_errno(),
            Errno::EINTR
        );
        assert_eq!(
            reverie::syscalls::Futex::new().kernel_restart_errno(),
            Errno::ERESTARTSYS
        );
        // A timed wait is never restarted after a handler runs. FUTEX_WAIT's timeout
        // is relative, so its deadline is kept as poll's is.
        let timeout = reverie::syscalls::Addr::from_raw(0x1000).unwrap();
        assert_eq!(
            reverie::syscalls::Futex::new()
                .with_timeout(Some(timeout))
                .kernel_restart_errno(),
            Errno::ERESTART_RESTARTBLOCK
        );
        // FUTEX_WAIT_BITSET's timeout is an absolute deadline, which Linux copies
        // into the restart block as it does FUTEX_WAIT's, so a deadline the guest
        // moves in memory after the wait began has no effect. Running the call
        // again, which ERESTARTNOHAND did, read the moved deadline (round-10
        // finding Medium 4 on https://github.com/rrnewton/hermit/pull/3361).
        let bitset = reverie::syscalls::Futex::new()
            .with_futex_op(libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG)
            .with_val3(-1);
        assert_eq!(bitset.kernel_restart_errno(), Errno::ERESTARTSYS);
        assert!(!bitset.restart_keeps_deadline());
        assert_eq!(
            bitset.with_timeout(Some(timeout)).kernel_restart_errno(),
            Errno::ERESTART_RESTARTBLOCK
        );
        assert!(bitset.with_timeout(Some(timeout)).restart_keeps_deadline());
    }

    #[test]
    fn a_held_signal_is_reported_unless_it_is_sigstop() {
        // The backend passes a held signal on unreported when the call returns
        // ERESTART_RESTARTBLOCK, so a held signal other than SIGSTOP ends the wait
        // with ERESTARTNOHAND, which a handler also turns into EINTR.
        for signal in [libc::SIGUSR1, libc::SIGTERM, libc::SIGTSTP, libc::SIGCHLD] {
            assert_eq!(
                held_signal_restart_errno(Errno::ERESTART_RESTARTBLOCK, signal),
                Errno::ERESTARTNOHAND,
                "signal {signal}"
            );
        }
        // SIGSTOP is never reported, and its restart must keep the deadline.
        assert_eq!(
            held_signal_restart_errno(Errno::ERESTART_RESTARTBLOCK, libc::SIGSTOP),
            Errno::ERESTART_RESTARTBLOCK
        );
        // Every other restart code is the call's own.
        for errno in [
            Errno::ERESTARTNOHAND,
            Errno::ERESTARTSYS,
            Errno::ERESTARTNOINTR,
            Errno::EINTR,
        ] {
            for signal in [libc::SIGUSR1, libc::SIGSTOP] {
                assert_eq!(held_signal_restart_errno(errno, signal), errno);
            }
        }
    }

    #[test]
    fn a_restart_block_resumes_only_at_the_interrupted_call() {
        let timeout = reverie::syscalls::Addr::from_raw(0x1000).unwrap();
        let block = RestartBlock {
            rip: 0x4000_1234,
            deadline: Some(LogicalTime::from_nanos(300_000_000)),
            call: RestartCall::Futex(reverie::syscalls::Futex::new().with_timeout(Some(timeout))),
        };
        // The kernel's restart runs restart_syscall at the interrupted call's address.
        assert_eq!(block.resumed_at(0x4000_1234), Some(block));
        // A restart_syscall anywhere else is the guest's own call.
        assert_eq!(block.resumed_at(0x4000_1236), None);
        assert_eq!(block.resumed_at(0x4000_1232), None);
    }

    #[test]
    fn writev_signal_result_uses_disposition_and_progress() {
        let call = reverie::syscalls::Writev::new();
        for disposition in [
            WaitSignalDisposition::Interrupt,
            WaitSignalDisposition::Restart,
        ] {
            assert!(matches!(
                interrupted_write_result(&call, 0, Some(disposition)),
                Some(Err(Error::Errno(Errno::ERESTARTSYS)))
            ));
            assert!(matches!(
                interrupted_write_result(&call, 17, Some(disposition)),
                Some(Ok(17))
            ));
        }
        assert!(interrupted_write_result(&call, 0, None).is_none());
        assert!(interrupted_write_result(&call, 17, None).is_none());
    }

    #[test]
    fn pipe_writev_requests_signal_disposition() {
        let dettid = DetTid::from_raw(42);
        let call = reverie::syscalls::Writev::new();
        let request = pipe_writev_resources(dettid, call);

        assert_eq!(
            request.signal_interrupt_errno(),
            Some(Errno::ERESTARTSYS.into_raw())
        );
        assert!(request.decides_signal_interruption_itself);
        assert_eq!(request.resources.len(), 1);
    }
}

/// `KernelSignalWait` against a scripted guest and a scripted `/proc` read
/// (`signal_state_read_seam`). No failure of the wait's signal handling may reach
/// the guest as an errno its wait call cannot return, and the guest must never
/// resume with every signal blocked
/// (https://github.com/rrnewton/hermit/issues/3146).
#[cfg(test)]
mod kernel_signal_wait_failures {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex;

    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Pid;
    use reverie::syscalls::LocalMemory;

    use super::*;
    use crate::Config;
    use crate::GlobalState;
    use crate::ThreadState;
    use crate::syscalls::threads::KernelSignalState;
    use crate::syscalls::threads::kernel_sigset_bit;
    use crate::syscalls::threads::signal_state_read_seam;
    use crate::types::DetPid;

    /// What an injected `rt_sigprocmask` does.
    #[derive(Clone, Copy, Debug)]
    enum MaskOutcome {
        /// The call runs: the mask changes and it returns 0.
        Apply,
        /// A signal stops the guest before the call runs: the mask is unchanged
        /// and the backend reports its restart errno.
        StoppedBefore,
        /// The call cannot run at all.
        Fail(Errno),
        /// `StoppedBefore`, by a signal that `/proc` never showed because it
        /// arrived after the read before the injection, such as a `SIGSTOP`
        /// sent from outside: the backend holds it in its single slot
        /// (`FakeKernel::held_slot`).
        StoppedBy(i32),
    }

    /// The kernel's view of the guest thread, shared by the scripted `/proc`
    /// read and the scripted guest.
    #[derive(Default)]
    struct FakeKernel {
        /// The thread's mask (`SigBlk`).
        blocked: KernelSigset,
        /// Pending signals (`SigPnd | ShdPnd`).
        pending: KernelSigset,
        /// Signals with a handler (`SigCgt`).
        caught: KernelSigset,
        /// Signals whose disposition is `SIG_IGN` (`SigIgn`).
        ignored: KernelSigset,
        /// Whether a pending signal outside the mask stops each injection before
        /// the call runs, as on ptrace, where an ignored signal still queues for
        /// a traced task: it leaves the queue for the backend to hold
        /// (`taken`), and the injection reports a restart errno.
        stops_for_pending: bool,
        /// The signals that stopped an injection, in order.
        taken: Vec<i32>,
        /// Errors for the next reads, one per read.
        read_failures: VecDeque<Errno>,
        /// Outcomes for the next `rt_sigprocmask` calls; `Apply` once empty.
        outcomes: VecDeque<MaskOutcome>,
        /// Every mask the guest was asked to set, in order.
        requested: Vec<KernelSigset>,
        /// How many more signals that stop an injection are sent again, as by
        /// a sender faster than the injections: each such signal is pending
        /// again from the read after the one that follows the stop, which still
        /// shows it gone, so the stop is identified (`pending_repost`).
        repost_after_take: usize,
        /// The signals sent again, which the read after the next one shows.
        pending_repost: KernelSigset,
        /// The backend's one slot for a held signal: each signal that stops an
        /// injection (`taken`, `MaskOutcome::StoppedBy`) replaces the one there,
        /// as Reverie's ptrace backend does (`hold_pending_signal`).
        held_slot: Option<i32>,
        /// The signals that a stop dropped from `held_slot`, in order. A standard
        /// signal that replaces itself is merged, as Linux merges a second one
        /// sent while the first is pending, and is not lost.
        lost: Vec<i32>,
    }

    impl FakeKernel {
        /// The backend holds `signal`, which stopped an injection.
        fn hold(&mut self, signal: i32) {
            if let Some(previous) = self.held_slot.replace(signal)
                && !(previous == signal && signal < KERNEL_SIGRTMIN)
            {
                self.lost.push(previous);
            }
        }
    }

    type Kernel = Arc<Mutex<FakeKernel>>;

    /// Room for one mask, 8-byte aligned like a real stack slot.
    const ARENA_WORDS: usize = 2;

    /// A scratch stack in this process's memory whose commit can fail like the
    /// ptrace scratch below an `rsp` with no writable memory under it
    /// (https://github.com/rrnewton/hermit/issues/3328).
    struct WaitStack {
        commit_fails: bool,
        arena: usize,
    }

    struct WaitStackGuard;

    impl Drop for WaitStackGuard {
        fn drop(&mut self) {}
    }

    impl reverie::Stack for WaitStack {
        type StackGuard = WaitStackGuard;

        fn size(&self) -> usize {
            panic!("a blocked wait must not query the scratch size")
        }
        fn capacity(&self) -> usize {
            panic!("a blocked wait must not query the scratch capacity")
        }
        fn push<'stack, T>(&mut self, value: T) -> Addr<'stack, T> {
            assert!(std::mem::size_of::<T>() <= ARENA_WORDS * std::mem::size_of::<u64>());
            // SAFETY: the arena is a live, 8-byte aligned buffer owned by the guest,
            // large enough for `T` (asserted above).
            unsafe { std::ptr::write(self.arena as *mut T, value) };
            Addr::from_raw(self.arena).unwrap()
        }
        fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
            panic!("a blocked wait pushes its mask rather than reserving room")
        }
        fn commit(self) -> Result<Self::StackGuard, Errno> {
            if self.commit_fails {
                Err(Errno::EFAULT)
            } else {
                Ok(WaitStackGuard)
            }
        }
    }

    struct WaitGuest {
        config: Config,
        thread: ThreadState<()>,
        pid: Pid,
        tid: Pid,
        commit_fails: bool,
        arena: Box<[u64; ARENA_WORDS]>,
        kernel: Kernel,
    }

    impl WaitGuest {
        /// A guest for thread `tid` of this process, with the guest's own `mask`
        /// installed.
        fn new(tid: Pid, mask: KernelSigset) -> (Self, Kernel) {
            let config = Config::default();
            let thread = ThreadState::new(DetPid::from_raw(1), &config, ());
            let kernel = Arc::new(Mutex::new(FakeKernel {
                blocked: mask,
                ..FakeKernel::default()
            }));
            let guest = Self {
                config,
                thread,
                pid: Pid::from_raw(std::process::id() as i32),
                tid,
                commit_fails: false,
                arena: Box::new([u64::MAX; ARENA_WORDS]),
                kernel: kernel.clone(),
            };
            (guest, kernel)
        }

        /// A guest for the calling thread, which exists.
        fn live(mask: KernelSigset) -> (Self, Kernel) {
            // SAFETY: gettid has no preconditions.
            Self::new(Pid::from_raw(unsafe { libc::gettid() }), mask)
        }
    }

    /// Answer this thread's `/proc` reads from `kernel` until the result drops.
    fn scripted_proc(kernel: &Kernel) -> signal_state_read_seam::Installed {
        let kernel = kernel.clone();
        signal_state_read_seam::install(move |_, _| {
            let mut kernel = kernel.lock().unwrap();
            let read = match kernel.read_failures.pop_front() {
                Some(errno) => Err(errno),
                None => Ok(KernelSignalState {
                    pending: kernel.pending,
                    thread_pending: kernel.pending,
                    blocked: kernel.blocked,
                    caught: kernel.caught,
                    ignored: kernel.ignored,
                    ..KernelSignalState::default()
                }),
            };
            kernel.pending |= std::mem::take(&mut kernel.pending_repost);
            Some(read)
        })
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for WaitGuest {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            panic!("these waits must not send an RPC: {:?}", message.2)
        }
        fn config(&self) -> &Config {
            &self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for WaitGuest {
        type Memory = LocalMemory;
        type Stack = WaitStack;

        fn tid(&self) -> Pid {
            self.tid
        }
        fn pid(&self) -> Pid {
            self.pid
        }
        fn ppid(&self) -> Option<Pid> {
            None
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
            panic!("a blocked wait must not read registers")
        }
        async fn stack(&mut self) -> Self::Stack {
            WaitStack {
                commit_fails: self.commit_fails,
                arena: self.arena.as_mut_ptr() as usize,
            }
        }
        async fn daemonize(&mut self) {
            panic!("a blocked wait must not daemonize")
        }
        async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
            let (number, args) = syscall.into_parts();
            let Syscall::RtSigprocmask(call) = Syscall::from_raw(number, args) else {
                panic!("a blocked wait injected {number}, not rt_sigprocmask")
            };
            assert_eq!(call.how(), libc::SIG_SETMASK);
            assert!(call.oldset().is_none());
            let set = call.set().expect("rt_sigprocmask without a mask");
            let mask: KernelSigset = LocalMemory::new().read_value(set.cast::<KernelSigset>())?;
            let mut kernel = self.kernel.lock().unwrap();
            kernel.requested.push(mask);
            let deliverable = kernel.pending & !kernel.blocked;
            if kernel.stops_for_pending && deliverable != 0 {
                let signal = deliverable.trailing_zeros() as i32 + 1;
                kernel.pending &= !kernel_sigset_bit(signal);
                kernel.taken.push(signal);
                kernel.hold(signal);
                if kernel.repost_after_take > 0 {
                    kernel.repost_after_take -= 1;
                    kernel.pending_repost |= kernel_sigset_bit(signal);
                }
                return Err(Errno::ERESTARTSYS);
            }
            match kernel.outcomes.pop_front().unwrap_or(MaskOutcome::Apply) {
                MaskOutcome::Apply => {
                    kernel.blocked = mask;
                    Ok(0)
                }
                MaskOutcome::StoppedBefore => Err(Errno::ERESTARTNOINTR),
                MaskOutcome::StoppedBy(signal) => {
                    kernel.hold(signal);
                    Err(Errno::ERESTARTNOINTR)
                }
                MaskOutcome::Fail(errno) => Err(errno),
            }
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
            panic!("a blocked wait must not retire the guest")
        }
        fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("a blocked wait must not set a timer")
        }
        fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("a blocked wait must not set a timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("a blocked wait must not read a clock")
        }
    }

    /// The guest's own mask in these tests.
    fn guest_mask() -> KernelSigset {
        kernel_sigset_bit(libc::SIGUSR2)
    }

    /// The diagnostic a result ends the run with.
    fn diagnostic<V: std::fmt::Debug>(result: Result<V, Error>) -> BlockedWaitSignalError {
        match result {
            Err(Error::Tool(error)) => *error
                .downcast_ref::<BlockedWaitSignalError>()
                .unwrap_or_else(|| panic!("not a blocked-wait diagnostic: {error:#}")),
            other => panic!("expected the run to end with a diagnostic, got {other:?}"),
        }
    }

    /// The held-signal refusal a result ends the wait with.
    fn refusal<V: std::fmt::Debug>(result: Result<V, Error>) -> HeldSignalRefusal {
        match result {
            Err(Error::Tool(error)) => *error
                .downcast_ref::<HeldSignalRefusal>()
                .unwrap_or_else(|| panic!("not a held-signal refusal: {error:#}")),
            other => panic!("expected the wait to end with a held-signal refusal, got {other:?}"),
        }
    }

    /// A wait whose guest mask `block` replaced with the all-blocked mask.
    async fn blocked_wait(guest: &mut WaitGuest, kernel: &Kernel) -> KernelSignalWait {
        let mut wait = KernelSignalWait::new(guest, 0, false, Errno::ERESTARTSYS);
        wait.block(guest, None).await.unwrap();
        assert!(!wait.needs_block());
        assert_eq!(kernel.lock().unwrap().blocked, blocked_signal_mask());
        wait
    }

    /// Each stopped attempt is a stop by a signal that `/proc` cannot identify,
    /// which the backend holds in place of the one before. `restore` used to
    /// return success here; the second stop drops the first unidentified one,
    /// which may matter, so the mask is still put back and the wait now ends
    /// with a `HeldSignalRefusal`, as a wait loop that meets two such stops
    /// does (round-11 High F1 on
    /// https://github.com/rrnewton/hermit/pull/3361).
    #[tokio::test]
    async fn restore_repeats_the_mask_change_until_the_kernel_reports_it() {
        let (mut guest, kernel) = WaitGuest::live(guest_mask());
        let _proc = scripted_proc(&kernel);
        let mut wait = blocked_wait(&mut guest, &kernel).await;
        kernel
            .lock()
            .unwrap()
            .outcomes
            .extend([MaskOutcome::StoppedBefore; 5]);

        let result = wait.restore(&mut guest, None).await;

        assert_eq!(
            refusal(result),
            HeldSignalRefusal {
                wait: None,
                held: None,
                replacing: None,
                loss: HeldSignalLoss::Restoration,
            }
        );
        let kernel = kernel.lock().unwrap();
        assert_eq!(kernel.blocked, guest_mask());
        assert_eq!(
            kernel.requested.len(),
            1 + 6,
            "block, then five stopped attempts and one that ran"
        );
        assert!(
            kernel.requested[1..]
                .iter()
                .all(|&mask| mask == guest_mask())
        );
    }

    #[tokio::test]
    async fn a_mask_that_signals_keep_stopping_ends_the_run_instead_of_resuming_the_guest() {
        let (mut guest, kernel) = WaitGuest::live(guest_mask());
        let _proc = scripted_proc(&kernel);
        let mut wait = blocked_wait(&mut guest, &kernel).await;
        kernel
            .lock()
            .unwrap()
            .outcomes
            .extend([MaskOutcome::StoppedBefore; 64]);

        let error = diagnostic(wait.restore(&mut guest, None).await);

        assert_eq!(
            error,
            BlockedWaitSignalError::MaskNotRestored {
                pid: guest.pid,
                tid: guest.tid,
                attempts: 16,
                last_error: Some(Errno::ERESTARTNOINTR),
            }
        );
        let message = error.to_string();
        assert!(
            message.starts_with("cannot restore the signal mask of guest thread")
                && message.contains("(16 attempts, last error ")
                && message.contains("ERESTARTNOINTR")
                && message.ends_with("); it would resume with every signal blocked"),
            "{message}"
        );
        assert_eq!(kernel.lock().unwrap().requested.len(), 1 + 16);
    }

    #[tokio::test]
    async fn a_mask_change_that_cannot_run_ends_the_run_instead_of_resuming_the_guest() {
        let (mut guest, kernel) = WaitGuest::live(guest_mask());
        let _proc = scripted_proc(&kernel);
        let mut wait = blocked_wait(&mut guest, &kernel).await;
        kernel
            .lock()
            .unwrap()
            .outcomes
            .push_back(MaskOutcome::Fail(Errno::EFAULT));

        let error = diagnostic(wait.restore(&mut guest, None).await);

        assert_eq!(
            error,
            BlockedWaitSignalError::MaskNotRestored {
                pid: guest.pid,
                tid: guest.tid,
                attempts: 1,
                last_error: Some(Errno::EFAULT),
            }
        );
    }

    #[tokio::test]
    async fn an_unreadable_signal_state_of_a_live_thread_ends_the_run() {
        let (mut guest, kernel) = WaitGuest::live(guest_mask());
        let _proc = scripted_proc(&kernel);
        let (pid, tid) = (guest.pid, guest.tid);
        let expected = |errno| BlockedWaitSignalError::StateUnreadable { pid, tid, errno };

        // The turn's check, before the mask is set.
        let mut wait = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTSYS);
        kernel.lock().unwrap().read_failures.push_back(Errno::EIO);
        assert_eq!(diagnostic(wait.interrupted()), expected(Errno::EIO));

        // `block`'s read of the guest's mask.
        kernel.lock().unwrap().read_failures.push_back(Errno::ESRCH);
        assert_eq!(
            diagnostic(wait.block(&mut guest, None).await),
            expected(Errno::ESRCH)
        );
        assert!(kernel.lock().unwrap().requested.is_empty());

        // `restore`'s read, with every signal blocked.
        wait.block(&mut guest, None).await.unwrap();
        kernel.lock().unwrap().read_failures.push_back(Errno::ESRCH);
        assert_eq!(
            diagnostic(wait.restore(&mut guest, None).await),
            expected(Errno::ESRCH)
        );
        let error = expected(Errno::EIO).to_string();
        assert!(
            error.starts_with("cannot read the signal state of guest thread"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_thread_that_no_longer_exists_ends_its_wait_with_erestartnointr() {
        // SAFETY: gettid has no preconditions.
        let exited = std::thread::spawn(|| unsafe { libc::gettid() })
            .join()
            .unwrap();
        // `join` returns when the kernel clears the thread's TID word, early in
        // its exit; the thread stays in `/proc` and answers `tgkill` until the
        // kernel releases it a moment later. Measured on the development host,
        // 62 of 20000 joined threads were still listed right after `join`, and
        // a GitHub-hosted run read such a thread's status here
        // (https://github.com/rrnewton/hermit/actions/runs/37683205050). So wait
        // for the release. The path is in this process's own task directory, so
        // no other process's thread can appear under it, and Linux hands out
        // TIDs cyclically, so this one is not reissued here before the counter
        // wraps.
        let task = format!("/proc/self/task/{exited}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::path::Path::new(&task).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "thread {exited} is still in /proc 10 s after it was joined"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let (guest, _kernel) = WaitGuest::new(Pid::from_raw(exited), guest_mask());
        // No script: the read goes to the real `/proc`, where the thread is gone.

        let wait = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTSYS);
        assert!(matches!(
            wait.interrupted(),
            Err(Error::Errno(Errno::ERESTARTNOINTR))
        ));
        assert!(matches!(
            read_wait_signal_state(guest.pid, guest.tid),
            Err(Error::Errno(Errno::ERESTARTNOINTR))
        ));
    }

    #[tokio::test]
    async fn a_stack_with_no_scratch_room_leaves_the_wait_under_the_guest_mask() {
        let (mut guest, kernel) = WaitGuest::live(guest_mask());
        guest.commit_fails = true;
        let _proc = scripted_proc(&kernel);
        let mut wait = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTSYS);

        wait.block(&mut guest, None).await.unwrap();

        assert!(!wait.needs_block(), "block must not be tried again");
        assert_eq!(kernel.lock().unwrap().blocked, guest_mask());
        assert!(kernel.lock().unwrap().requested.is_empty());

        // Later turns classify against the guest's own mask, read each turn.
        {
            let mut kernel = kernel.lock().unwrap();
            kernel.caught = kernel_sigset_bit(libc::SIGUSR1) | kernel_sigset_bit(libc::SIGUSR2);
            kernel.pending = kernel_sigset_bit(libc::SIGUSR2);
        }
        assert!(!wait.interrupted().unwrap(), "SIGUSR2 is blocked");
        kernel.lock().unwrap().pending = kernel_sigset_bit(libc::SIGUSR1);
        assert!(wait.interrupted().unwrap(), "SIGUSR1 is not blocked");

        wait.restore(&mut guest, None).await.unwrap();
        assert!(
            kernel.lock().unwrap().requested.is_empty(),
            "nothing to put back"
        );
        assert_eq!(kernel.lock().unwrap().blocked, guest_mask());
    }

    /// A guest whose injections a pending signal outside the mask stops, as on
    /// ptrace, with `pending` queued and `ignored` set to `SIG_IGN`.
    fn stopping_guest(pending: KernelSigset, ignored: KernelSigset) -> (WaitGuest, Kernel) {
        let (guest, kernel) = WaitGuest::live(guest_mask());
        {
            let mut kernel = kernel.lock().unwrap();
            kernel.stops_for_pending = true;
            kernel.pending = pending;
            kernel.ignored = ignored;
        }
        (guest, kernel)
    }

    /// Inject a mask change to `mask` through `wait.inject_absorbing`.
    async fn inject_mask_absorbing(
        wait: &mut KernelSignalWait,
        guest: &mut WaitGuest,
        mask: KernelSigset,
    ) -> Result<Result<i64, Errno>, Error> {
        let mut stack = guest.stack().await;
        let cell = stack.push(mask);
        let _guard = stack.commit().unwrap();
        wait.inject_absorbing(guest, set_signal_mask_call(cell))
            .await
    }

    /// An ignored signal that was already pending stops the mask change of
    /// `block`, which used to end the wait with a restart (review finding F2 on
    /// https://github.com/rrnewton/hermit/pull/3361). Linux would not end the
    /// wait for it, so the stop is absorbed: the backend holds the signal, the
    /// change runs again, and the held signal does not interrupt the wait.
    #[tokio::test]
    async fn an_ignored_signal_that_stops_the_mask_change_does_not_end_the_wait() {
        let usr1 = kernel_sigset_bit(libc::SIGUSR1);
        let (mut guest, kernel) = stopping_guest(usr1, usr1);
        let _proc = scripted_proc(&kernel);
        let mut wait = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTSYS);

        wait.block(&mut guest, None).await.unwrap();

        assert!(!wait.needs_block());
        let all_blocked = blocked_signal_mask() | guest_mask();
        {
            let kernel = kernel.lock().unwrap();
            assert_eq!(kernel.taken, vec![libc::SIGUSR1]);
            assert_eq!(kernel.requested, vec![all_blocked, all_blocked]);
            assert_eq!(kernel.blocked, all_blocked);
        }
        assert_eq!(
            wait.held,
            Some(HeldSignal {
                signal: libc::SIGUSR1,
                queue: SignalQueue::Thread,
                kind: HeldKind::Harmless,
            })
        );
        assert!(
            !wait.interrupted().unwrap(),
            "an ignored signal does not end the wait"
        );
        wait.restore(&mut guest, None).await.unwrap();
        assert_eq!(kernel.lock().unwrap().blocked, guest_mask());
    }

    /// A default-ignored signal that stops an injection is held the same way,
    /// and the injection then returns its own result (review finding F2 on
    /// https://github.com/rrnewton/hermit/pull/3361).
    #[tokio::test]
    async fn a_default_ignored_signal_that_stops_an_injection_is_held() {
        let (mut guest, kernel) = stopping_guest(kernel_sigset_bit(libc::SIGWINCH), 0);
        let _proc = scripted_proc(&kernel);
        let mut wait = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTSYS);

        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

        assert!(matches!(result, Ok(Ok(0))), "{result:?}");
        assert_eq!(kernel.lock().unwrap().taken, vec![libc::SIGWINCH]);
        assert_eq!(kernel.lock().unwrap().requested.len(), 2);
        assert_eq!(wait.held.map(|held| held.kind), Some(HeldKind::Harmless));
        assert!(!wait.interrupted().unwrap());
    }

    /// A pending signal that would end the wait natively ends it before any
    /// injection, with the restart errno, and stays queued for the guest.
    #[tokio::test]
    async fn a_pending_caught_signal_ends_the_wait_before_the_injection() {
        let usr1 = kernel_sigset_bit(libc::SIGUSR1);
        let (mut guest, kernel) = stopping_guest(usr1, 0);
        kernel.lock().unwrap().caught = usr1;
        let _proc = scripted_proc(&kernel);
        let mut wait = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTSYS);

        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

        assert!(
            matches!(result, Err(Error::Errno(Errno::ERESTARTSYS))),
            "{result:?}"
        );
        let kernel = kernel.lock().unwrap();
        assert!(kernel.requested.is_empty(), "nothing is injected");
        assert_eq!(kernel.pending, usr1);
        assert_eq!(wait.held, None);
    }

    /// The backend holds one signal. Once it holds one whose loss would matter,
    /// a `SIGCHLD` here, an injection that a different pending signal would stop
    /// is not made. The wait used to end with `ERESTARTNOINTR`, a transparent
    /// restart; it now ends with a `HeldSignalRefusal`, which its caller turns
    /// into the unsupported-operation refusal (round-10 High 1 on
    /// https://github.com/rrnewton/hermit/pull/3361).
    #[tokio::test]
    async fn a_held_sigchld_is_never_replaced_by_another_stop() {
        let (mut guest, kernel) = stopping_guest(
            kernel_sigset_bit(libc::SIGCHLD) | kernel_sigset_bit(libc::SIGWINCH),
            0,
        );
        let _proc = scripted_proc(&kernel);
        let mut wait = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTSYS);

        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

        assert_eq!(
            refusal(result),
            HeldSignalRefusal {
                wait: None,
                held: Some(libc::SIGCHLD),
                replacing: Some(libc::SIGWINCH),
                loss: HeldSignalLoss::Replaced,
            }
        );
        let kernel = kernel.lock().unwrap();
        assert_eq!(kernel.taken, vec![libc::SIGCHLD]);
        assert_eq!(
            kernel.requested.len(),
            1,
            "the second injection is not made"
        );
        assert_eq!(kernel.pending, kernel_sigset_bit(libc::SIGWINCH));
        assert_eq!(wait.held.map(|held| held.kind), Some(HeldKind::Precious));
    }

    /// A wait whose `block` was stopped by a default `SIGCHLD`, which the
    /// backend holds as a signal whose loss matters (`HeldKind::Precious`).
    async fn wait_holding_sigchld(guest: &mut WaitGuest, kernel: &Kernel) -> KernelSignalWait {
        let mut wait = KernelSignalWait::new(&*guest, 0, false, Errno::ERESTARTSYS);
        wait.block(guest, None).await.unwrap();
        assert_eq!(
            wait.held.map(|held| (held.signal, held.kind)),
            Some((libc::SIGCHLD, HeldKind::Precious))
        );
        let kernel = kernel.lock().unwrap();
        assert_eq!(kernel.blocked, blocked_signal_mask() | guest_mask());
        assert_eq!(kernel.held_slot, Some(libc::SIGCHLD));
        wait
    }

    /// A signal that `/proc` never showed, a `SIGSTOP` sent from outside here,
    /// stops the injection that puts the guest's mask back before it runs, and
    /// the next attempt runs. The backend holds one signal, so the `SIGSTOP`
    /// replaced the held `SIGCHLD`, and `restore` used to return success, so
    /// the guest resumed without its `SIGCHLD` (round-11 High F1 on
    /// https://github.com/rrnewton/hermit/pull/3361). The mask is still put
    /// back, and the wait now ends with a `HeldSignalRefusal`.
    #[tokio::test]
    async fn a_stop_of_the_mask_restoration_that_replaces_a_held_sigchld_is_refused() {
        let (mut guest, kernel) = stopping_guest(kernel_sigset_bit(libc::SIGCHLD), 0);
        let _proc = scripted_proc(&kernel);
        let mut wait = wait_holding_sigchld(&mut guest, &kernel).await;
        kernel
            .lock()
            .unwrap()
            .outcomes
            .push_back(MaskOutcome::StoppedBy(libc::SIGSTOP));

        let result = wait.restore(&mut guest, None).await;

        {
            let kernel = kernel.lock().unwrap();
            assert_eq!(kernel.blocked, guest_mask(), "the guest's mask is back");
            assert_eq!(
                kernel.requested[2..],
                [guest_mask(), guest_mask()],
                "one stopped restoration attempt and one that ran"
            );
            assert_eq!(kernel.held_slot, Some(libc::SIGSTOP));
            assert_eq!(kernel.lost, vec![libc::SIGCHLD]);
        }
        assert_eq!(
            refusal(result),
            HeldSignalRefusal {
                wait: None,
                held: Some(libc::SIGCHLD),
                replacing: None,
                loss: HeldSignalLoss::Restoration,
            }
        );
    }

    /// The same with a signal that `/proc` shows before the restoration and
    /// that the all-blocked mask leaves unblocked: signal 32, which the C
    /// library reserves and leaves out of the blockable set. The refusal names
    /// it.
    #[tokio::test]
    async fn an_identified_stop_of_the_mask_restoration_that_replaces_a_held_sigchld_is_refused() {
        const RESERVED: i32 = 32;
        assert_eq!(blocked_signal_mask() & kernel_sigset_bit(RESERVED), 0);
        let (mut guest, kernel) = stopping_guest(kernel_sigset_bit(libc::SIGCHLD), 0);
        let _proc = scripted_proc(&kernel);
        let mut wait = wait_holding_sigchld(&mut guest, &kernel).await;
        kernel.lock().unwrap().pending = kernel_sigset_bit(RESERVED);

        let result = wait.restore(&mut guest, None).await;

        {
            let kernel = kernel.lock().unwrap();
            assert_eq!(kernel.blocked, guest_mask(), "the guest's mask is back");
            assert_eq!(kernel.taken, vec![libc::SIGCHLD, RESERVED]);
            assert_eq!(kernel.held_slot, Some(RESERVED));
            assert_eq!(kernel.lost, vec![libc::SIGCHLD]);
        }
        let refused = refusal(result);
        assert_eq!(
            refused,
            HeldSignalRefusal {
                wait: None,
                held: Some(libc::SIGCHLD),
                replacing: Some(RESERVED),
                loss: HeldSignalLoss::Restoration,
            }
        );
        let message = refused.to_string();
        assert!(
            message.starts_with(
                "unsupported: the a blocking wait cannot keep the held signal SIGCHLD: signal 32 \
                 stopped the injection that puts the guest's signal mask back and replaced it"
            ),
            "{message}"
        );
    }

    /// A refusal from the mask restoration replaces the wait's result, whether
    /// that is a value or an errno, unless the wait already ended with a refusal
    /// of its own, which came first. A mask that could not be put back still
    /// ends the run.
    #[test]
    fn a_restoration_refusal_replaces_the_result_unless_the_wait_refused_first() {
        let tool = |refusal: HeldSignalRefusal| Error::Tool(anyhow::Error::new(refusal));
        let restoration = HeldSignalRefusal {
            wait: None,
            held: Some(libc::SIGCHLD),
            replacing: None,
            loss: HeldSignalLoss::Restoration,
        };
        let replaced = HeldSignalRefusal {
            wait: None,
            held: Some(libc::SIGCHLD),
            replacing: Some(libc::SIGUSR1),
            loss: HeldSignalLoss::Replaced,
        };
        let not_restored = BlockedWaitSignalError::MaskNotRestored {
            pid: Pid::from_raw(1),
            tid: Pid::from_raw(1),
            attempts: 16,
            last_error: Some(Errno::ERESTARTNOINTR),
        };

        assert_eq!(result_after_restore(Ok(()), Ok(3)).unwrap().unwrap(), 3);
        assert_eq!(
            refusal(result_after_restore(Err(tool(restoration)), Ok(3)).unwrap()),
            restoration
        );
        assert_eq!(
            refusal(
                result_after_restore(Err(tool(restoration)), Err(Errno::ERESTARTSYS.into()))
                    .unwrap()
            ),
            restoration
        );
        assert_eq!(
            refusal(result_after_restore(Err(tool(restoration)), Err(tool(replaced))).unwrap()),
            replaced
        );
        assert_eq!(
            diagnostic(result_after_restore(
                Err(Error::Tool(anyhow::Error::new(not_restored))),
                Ok(3)
            )),
            not_restored
        );
    }

    /// A guest whose next `stops` injections are stopped by a signal that
    /// `/proc` never shows, as one that arrives after the read before an
    /// injection and that the backend then holds: nothing leaves a queue, so the
    /// stop cannot be identified.
    fn unidentified_stops_guest(stops: usize) -> (WaitGuest, Kernel) {
        let (guest, kernel) = WaitGuest::live(guest_mask());
        kernel.lock().unwrap().outcomes =
            std::iter::repeat_n(MaskOutcome::StoppedBefore, stops).collect();
        (guest, kernel)
    }

    /// A stop that cannot be identified, such as a default-ignored `SIGCHLD`
    /// that arrives after the read, used to end the wait with `ERESTARTNOHAND`
    /// for a call whose restart code is not a kernel restart code. With no
    /// handler to run, the call then restarted, and a relative `poll` timeout
    /// or a timed polling `FUTEX_WAIT` started again with a fresh deadline
    /// (round-8 High 1 on https://github.com/rrnewton/hermit/pull/3361). The
    /// stop is now held and the injection runs again, so the wait goes on with
    /// its absolute deadline, as the blind retry before this machinery did.
    #[tokio::test]
    async fn a_stop_that_cannot_be_identified_keeps_a_timed_wait_and_its_deadline() {
        for deadline in [true, false] {
            let (mut guest, kernel) = unidentified_stops_guest(1);
            let _proc = scripted_proc(&kernel);
            let mut wait = KernelSignalWait::new(&guest, 0, true, Errno::ERESTARTNOHAND)
                .with_deadline(deadline);

            let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

            assert!(
                matches!(result, Ok(Ok(0))),
                "deadline={deadline}: {result:?}"
            );
            assert_eq!(
                kernel.lock().unwrap().requested.len(),
                2,
                "deadline={deadline}"
            );
            assert_eq!(wait.held, None, "deadline={deadline}");
        }
    }

    /// A wait with a deadline absorbs stops without the bound that ends any
    /// other wait after `MAX_ABSORBED_STOPS`, because a restart would start its
    /// timeout again (round-8 High 1 on
    /// https://github.com/rrnewton/hermit/pull/3361). A wait without one still
    /// ends at the bound, and, while the backend holds only a signal whose loss
    /// does not matter (an ignored `SIGUSR1` sent again and again here), with
    /// `ERESTARTNOINTR`: it is not refused.
    #[tokio::test]
    async fn only_a_wait_without_a_deadline_ends_after_the_absorbed_stop_bound() {
        let stops = 100;
        let usr1 = kernel_sigset_bit(libc::SIGUSR1);
        let (mut guest, kernel) = stopping_guest(usr1, usr1);
        kernel.lock().unwrap().repost_after_take = stops - 1;
        let _proc = scripted_proc(&kernel);
        let mut wait =
            KernelSignalWait::new(&guest, 0, true, Errno::ERESTARTNOHAND).with_deadline(true);
        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;
        assert!(matches!(result, Ok(Ok(0))), "{result:?}");
        assert_eq!(kernel.lock().unwrap().requested.len(), stops + 1);
        assert_eq!(wait.held.map(|held| held.kind), Some(HeldKind::Harmless));

        let (mut guest, kernel) = stopping_guest(usr1, usr1);
        kernel.lock().unwrap().repost_after_take = stops - 1;
        let _proc = scripted_proc(&kernel);
        let mut wait = KernelSignalWait::new(&guest, 0, true, Errno::ERESTARTNOHAND);
        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::ERESTARTNOINTR))),
            "{result:?}"
        );
        assert_eq!(kernel.lock().unwrap().requested.len(), 65);
    }

    /// At the same bound, a wait without a deadline whose backend holds a
    /// signal whose loss matters (a default `SIGCHLD`, sent again and again)
    /// used to end with `ERESTARTNOINTR` too. It now ends with a
    /// `HeldSignalRefusal` (round-10 High 1 on
    /// https://github.com/rrnewton/hermit/pull/3361).
    #[tokio::test]
    async fn a_wait_that_reaches_the_stop_bound_holding_a_sigchld_is_refused() {
        let (mut guest, kernel) = stopping_guest(kernel_sigset_bit(libc::SIGCHLD), 0);
        kernel.lock().unwrap().repost_after_take = 100;
        let _proc = scripted_proc(&kernel);
        let mut wait = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTSYS);

        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

        assert_eq!(
            refusal(result),
            HeldSignalRefusal {
                wait: None,
                held: Some(libc::SIGCHLD),
                replacing: None,
                loss: HeldSignalLoss::StopBound,
            }
        );
        assert_eq!(kernel.lock().unwrap().requested.len(), 65);
    }

    /// At the same bound, a stop that cannot be identified, after stops by a
    /// signal whose loss does not matter (an ignored `SIGUSR1` sent again and
    /// again), used to end a wait without a deadline with `ERESTARTNOINTR`,
    /// which let the backend deliver the unidentified signal, possibly a
    /// `SIGCHLD`, at a moment host timing could set. It now ends with a
    /// `HeldSignalRefusal`; a wait with a deadline, which has no bound, goes on
    /// (round-10 High 1 on https://github.com/rrnewton/hermit/pull/3361).
    #[tokio::test]
    async fn an_unidentified_stop_at_the_stop_bound_is_refused() {
        let usr1 = kernel_sigset_bit(libc::SIGUSR1);
        for deadline in [false, true] {
            let (mut guest, kernel) = stopping_guest(usr1, usr1);
            {
                let mut kernel = kernel.lock().unwrap();
                // 64 identified stops, the bound, then one that `/proc` never
                // shows.
                kernel.repost_after_take = 63;
                kernel.outcomes = VecDeque::from([MaskOutcome::StoppedBefore]);
            }
            let _proc = scripted_proc(&kernel);
            let mut wait = KernelSignalWait::new(&guest, 0, true, Errno::ERESTARTNOHAND)
                .with_deadline(deadline);

            let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

            if deadline {
                assert!(matches!(result, Ok(Ok(0))), "{result:?}");
                assert_eq!(kernel.lock().unwrap().requested.len(), 66);
                assert!(wait.held_unidentified);
            } else {
                assert_eq!(
                    refusal(result),
                    HeldSignalRefusal {
                        wait: None,
                        held: None,
                        replacing: None,
                        loss: HeldSignalLoss::StopBound,
                    }
                );
                assert_eq!(kernel.lock().unwrap().requested.len(), 65);
            }
            assert_eq!(
                kernel.lock().unwrap().taken.len(),
                64,
                "deadline={deadline}"
            );
        }
    }

    /// A stop that cannot be identified while the backend holds a `SIGCHLD`
    /// replaced the `SIGCHLD` in the backend's one slot. The wait used to go on
    /// and lose it; it now ends with a `HeldSignalRefusal`, with or without a
    /// deadline (round-10 High 1 on https://github.com/rrnewton/hermit/pull/3361).
    #[tokio::test]
    async fn an_unidentified_stop_over_a_held_sigchld_is_refused() {
        for deadline in [true, false] {
            let (mut guest, kernel) = stopping_guest(kernel_sigset_bit(libc::SIGCHLD), 0);
            kernel.lock().unwrap().outcomes = VecDeque::from([MaskOutcome::StoppedBefore]);
            let _proc = scripted_proc(&kernel);
            let mut wait =
                KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTSYS).with_deadline(deadline);

            let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

            assert_eq!(
                refusal(result),
                HeldSignalRefusal {
                    wait: None,
                    held: Some(libc::SIGCHLD),
                    replacing: None,
                    loss: HeldSignalLoss::UnidentifiedStop,
                },
                "deadline={deadline}"
            );
            let kernel = kernel.lock().unwrap();
            assert_eq!(kernel.requested.len(), 2, "deadline={deadline}");
            assert_eq!(kernel.taken, vec![libc::SIGCHLD], "deadline={deadline}");
        }
    }

    /// A second stop that cannot be identified replaced a first one, which may
    /// have been a signal whose loss matters. The wait used to go on; it now
    /// ends with a `HeldSignalRefusal`, with or without a deadline (round-10
    /// High 1 on https://github.com/rrnewton/hermit/pull/3361).
    #[tokio::test]
    async fn a_second_unidentified_stop_is_refused() {
        for deadline in [true, false] {
            let (mut guest, kernel) = unidentified_stops_guest(2);
            let _proc = scripted_proc(&kernel);
            let mut wait = KernelSignalWait::new(&guest, 0, true, Errno::ERESTARTNOHAND)
                .with_deadline(deadline);

            let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

            assert_eq!(
                refusal(result),
                HeldSignalRefusal {
                    wait: None,
                    held: None,
                    replacing: None,
                    loss: HeldSignalLoss::UnidentifiedStop,
                },
                "deadline={deadline}"
            );
            assert_eq!(
                kernel.lock().unwrap().requested.len(),
                2,
                "deadline={deadline}"
            );
        }
    }

    /// A `SIGCHLD` that arrives after the read before an injection stops it in a
    /// way that cannot be identified, and a second one, pending at the next
    /// read, would stop the injection made again and replace the first in the
    /// backend's one slot. The first may have been a signal whose loss matters,
    /// so the wait is refused before that injection, with or without a
    /// deadline (round-10 High 1 on https://github.com/rrnewton/hermit/pull/3361),
    /// and the backend still holds the first signal. This is the refusal a
    /// flood of external `SIGCHLD` racing a timed futex wait's first injections
    /// met on hermit main ccbe5ae8 (`external_signal_interrupt`'s test of the
    /// flood, now
    /// `ptrace_timed_futex_wait_and_poll_keep_their_deadline_after_a_sigchld_flood`,
    /// which sends its flood before the wait starts).
    #[tokio::test]
    async fn a_sigchld_after_an_unidentified_stop_is_refused_before_it_replaces_it() {
        let chld = kernel_sigset_bit(libc::SIGCHLD);
        for deadline in [true, false] {
            let (mut guest, kernel) = stopping_guest(0, 0);
            {
                let mut kernel = kernel.lock().unwrap();
                // Pending only after the first read: the injection's stop is not
                // identified. Sent once more, so it is pending again at the read
                // before the injection made again.
                kernel.pending_repost = chld;
                kernel.repost_after_take = 1;
            }
            let _proc = scripted_proc(&kernel);
            let mut wait = KernelSignalWait::new(&guest, 0, true, Errno::ERESTARTNOHAND)
                .with_deadline(deadline);

            let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

            let refused = refusal(result);
            assert_eq!(
                refused,
                HeldSignalRefusal {
                    wait: None,
                    held: None,
                    replacing: Some(libc::SIGCHLD),
                    loss: HeldSignalLoss::Replaced,
                },
                "deadline={deadline}"
            );
            let message = HeldSignalRefusal {
                wait: Some(Sysno::futex),
                ..refused
            }
            .to_string();
            assert!(
                message.starts_with(
                    "unsupported: the futex wait cannot keep a held signal that could not be \
                     identified: the pending SIGCHLD would stop the next injection and replace it"
                ),
                "deadline={deadline}: {message}"
            );
            assert!(wait.held_unidentified, "deadline={deadline}");
            let kernel = kernel.lock().unwrap();
            assert_eq!(
                kernel.requested.len(),
                1,
                "deadline={deadline}: the second injection is not made"
            );
            assert_eq!(kernel.taken, vec![libc::SIGCHLD], "deadline={deadline}");
            assert_eq!(kernel.held_slot, Some(libc::SIGCHLD), "deadline={deadline}");
            assert!(kernel.lost.is_empty(), "deadline={deadline}");
            assert_eq!(kernel.pending, chld, "deadline={deadline}");
        }
    }

    /// In a wait with a deadline, a held `SIGCHLD` that would not end the wait
    /// is not dropped when another pending signal would stop the next
    /// injection. The injection used to be made, and the stop replaced the held
    /// signal, as the blind retry allowed (round-8 High 1 on
    /// https://github.com/rrnewton/hermit/pull/3361); a restart would start the
    /// timeout again. The wait now ends with a `HeldSignalRefusal` before the
    /// injection (round-10 High 1 on the same pull request).
    #[tokio::test]
    async fn a_timed_wait_refuses_to_let_another_stop_replace_a_held_sigchld() {
        let (mut guest, kernel) = stopping_guest(
            kernel_sigset_bit(libc::SIGCHLD) | kernel_sigset_bit(libc::SIGWINCH),
            0,
        );
        let _proc = scripted_proc(&kernel);
        let mut wait =
            KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTNOHAND).with_deadline(true);

        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

        assert_eq!(
            refusal(result),
            HeldSignalRefusal {
                wait: None,
                held: Some(libc::SIGCHLD),
                replacing: Some(libc::SIGWINCH),
                loss: HeldSignalLoss::Replaced,
            }
        );
        let kernel = kernel.lock().unwrap();
        assert_eq!(kernel.taken, vec![libc::SIGCHLD]);
        assert_eq!(
            kernel.requested.len(),
            1,
            "the second injection is not made"
        );
        assert_eq!(kernel.pending, kernel_sigset_bit(libc::SIGWINCH));
    }

    /// In a wait with a deadline, a held signal that would end the wait natively
    /// ends it with the restart errno before another stop can replace it, as
    /// Linux ends the wait for that signal.
    #[tokio::test]
    async fn a_timed_wait_ends_for_a_held_signal_that_would_end_it() {
        let usr1 = kernel_sigset_bit(libc::SIGUSR1);
        let (mut guest, kernel) = stopping_guest(usr1 | kernel_sigset_bit(libc::SIGWINCH), 0);
        let _proc = scripted_proc(&kernel);
        let mut wait =
            KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTNOHAND).with_deadline(true);
        wait.held = Some(HeldSignal {
            signal: libc::SIGUSR1,
            queue: SignalQueue::Thread,
            kind: HeldKind::Precious,
        });
        kernel.lock().unwrap().pending = kernel_sigset_bit(libc::SIGWINCH);
        kernel.lock().unwrap().caught = usr1;

        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

        assert!(
            matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND))),
            "{result:?}"
        );
        assert!(
            kernel.lock().unwrap().requested.is_empty(),
            "nothing is injected"
        );
    }

    /// In a wait without a deadline, a held signal that would end the wait
    /// natively also ends it with the restart errno before another stop can
    /// replace it, as Linux ends the wait for that signal. It used to end with
    /// `ERESTARTNOINTR`, which restarted the call after the handler ran
    /// (round-10 High 1 on https://github.com/rrnewton/hermit/pull/3361).
    #[tokio::test]
    async fn a_wait_without_a_deadline_ends_for_a_held_signal_that_would_end_it() {
        let usr1 = kernel_sigset_bit(libc::SIGUSR1);
        let (mut guest, kernel) = stopping_guest(kernel_sigset_bit(libc::SIGWINCH), 0);
        kernel.lock().unwrap().caught = usr1;
        let _proc = scripted_proc(&kernel);
        let mut wait = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTSYS);
        wait.held = Some(HeldSignal {
            signal: libc::SIGUSR1,
            queue: SignalQueue::Thread,
            kind: HeldKind::Precious,
        });

        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

        assert!(
            matches!(result, Err(Error::Errno(Errno::ERESTARTSYS))),
            "{result:?}"
        );
        assert!(
            kernel.lock().unwrap().requested.is_empty(),
            "nothing is injected"
        );
    }

    /// A signal that a wait without a deadline consumes and that stops an
    /// injection is delivered rather than returned. One whose loss does not
    /// matter still ends the wait with `ERESTARTNOINTR`, and is not refused;
    /// a `SIGCHLD`, which the wait holds until the call returns, used to end it
    /// the same way and now ends it with a `HeldSignalRefusal` (round-10 High 1
    /// on https://github.com/rrnewton/hermit/pull/3361).
    #[tokio::test]
    async fn a_consumed_sigchld_that_stops_an_injection_is_refused() {
        let usr1 = kernel_sigset_bit(libc::SIGUSR1);
        let (mut guest, kernel) = stopping_guest(usr1, 0);
        let _proc = scripted_proc(&kernel);
        let mut wait = KernelSignalWait::new(&guest, usr1, false, Errno::ERESTARTSYS);
        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::ERESTARTNOINTR))),
            "{result:?}"
        );
        assert_eq!(kernel.lock().unwrap().taken, vec![libc::SIGUSR1]);
        drop(_proc);

        let chld = kernel_sigset_bit(libc::SIGCHLD);
        let (mut guest, kernel) = stopping_guest(chld, 0);
        let _proc = scripted_proc(&kernel);
        let mut wait = KernelSignalWait::new(&guest, chld, false, Errno::ERESTARTSYS);
        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;
        assert_eq!(
            refusal(result),
            HeldSignalRefusal {
                wait: None,
                held: None,
                replacing: Some(libc::SIGCHLD),
                loss: HeldSignalLoss::Consumed,
            }
        );
        assert_eq!(kernel.lock().unwrap().taken, vec![libc::SIGCHLD]);
    }

    /// A stop by a signal outside every class above, such as the backend's own
    /// preemption signal, still ends a wait without a deadline with
    /// `ERESTARTNOINTR`: the backend holds nothing whose loss matters, so it is
    /// not refused.
    #[tokio::test]
    async fn a_stop_by_the_preemption_signal_still_restarts_a_wait_without_a_deadline() {
        let signal = reverie::PERF_EVENT_SIGNAL as i32;
        let (mut guest, kernel) = stopping_guest(kernel_sigset_bit(signal), 0);
        let _proc = scripted_proc(&kernel);
        let mut wait = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTSYS);

        let result = inject_mask_absorbing(&mut wait, &mut guest, guest_mask()).await;

        assert!(
            matches!(result, Err(Error::Errno(Errno::ERESTARTNOINTR))),
            "{result:?}"
        );
        assert_eq!(kernel.lock().unwrap().taken, vec![signal]);
    }

    /// A wait that ends with a `HeldSignalRefusal` is refused through the
    /// unsupported-operation policy that `refuse_unserviceable_operation`
    /// applies: a run that is not fail-closed gets the `ERESTARTNOINTR` restart
    /// it got before, and a fail-closed one with `exit_on_unsupported_syscall`
    /// ends with the typed `UnsupportedSyscallError` for the wait's call. Every
    /// other result passes through, and the message names the wait, the
    /// signals and the pull request.
    #[tokio::test]
    async fn a_held_signal_refusal_follows_the_unsupported_operation_policy() {
        let refused = || {
            Err::<i64, Error>(Error::Tool(anyhow::Error::new(HeldSignalRefusal {
                wait: None,
                held: Some(libc::SIGCHLD),
                replacing: Some(libc::SIGWINCH),
                loss: HeldSignalLoss::Replaced,
            })))
        };
        let (mut guest, _kernel) = WaitGuest::live(guest_mask());
        guest.config.panic_on_unsupported_syscalls = false;
        let result = refuse_held_signal_loss(&mut guest, Sysno::poll, refused()).await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::ERESTARTNOINTR))),
            "{result:?}"
        );
        let result = refuse_held_signal_loss(&mut guest, Sysno::poll, Ok(3)).await;
        assert!(matches!(result, Ok(3)), "{result:?}");
        let result =
            refuse_held_signal_loss(&mut guest, Sysno::poll, Err(Errno::ERESTARTSYS.into())).await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::ERESTARTSYS))),
            "{result:?}"
        );

        guest.config.panic_on_unsupported_syscalls = true;
        guest.config.exit_on_unsupported_syscall = true;
        guest.config.shutdown_on_unsupported_syscall = false;
        let result = refuse_held_signal_loss(&mut guest, Sysno::poll, refused()).await;
        match result {
            Err(Error::Tool(error)) => assert!(
                matches!(
                    error.downcast_ref::<crate::UnsupportedSyscallError>(),
                    Some(crate::UnsupportedSyscallError(Sysno::poll))
                ),
                "{error:#}"
            ),
            other => panic!("expected the typed refusal, got {other:?}"),
        }

        let message = HeldSignalRefusal {
            wait: Some(Sysno::poll),
            held: Some(libc::SIGCHLD),
            replacing: Some(libc::SIGWINCH),
            loss: HeldSignalLoss::Replaced,
        }
        .to_string();
        for part in [
            "poll",
            "SIGCHLD",
            "SIGWINCH",
            "https://github.com/rrnewton/hermit/pull/3361",
        ] {
            assert!(message.contains(part), "{part} missing from: {message}");
        }
    }

    /// A guest with a caught `SIGCHLD` pending, outside its mask. With
    /// `gated_backend`, its configuration is the one under which the scheduler
    /// models signal targets: serialized threads and a backend that reports the
    /// kernel's signal state.
    fn caught_sigchld_guest(gated_backend: bool) -> (WaitGuest, Kernel) {
        let (mut guest, kernel) = WaitGuest::live(guest_mask());
        guest.config.sequentialize_threads = gated_backend;
        guest
            .config
            .backend_supports_blocked_wait_signal_interruption = gated_backend;
        {
            let mut kernel = kernel.lock().unwrap();
            kernel.pending = kernel_sigset_bit(libc::SIGCHLD);
            kernel.caught = kernel_sigset_bit(libc::SIGCHLD);
        }
        (guest, kernel)
    }

    /// A caught `SIGCHLD` pending outside the guest's mask, which the scheduler
    /// did not commit in this turn (`admit_committed`), never ends a gated
    /// wait, on any backend and whoever sent it: `/proc` cannot tell a
    /// `SIGCHLD` a guest or the scheduler sent at a deterministic point from the
    /// one the kernel posts for a child event at a moment set by host timing
    /// (https://github.com/rrnewton/hermit/issues/3146). The signal stays queued
    /// and is delivered when the call returns. The waits of `select` and
    /// `pselect6` hold nothing and end for it, as Linux ends them for any caught
    /// signal (`core_sys_select`).
    #[tokio::test]
    async fn a_pending_caught_sigchld_ends_a_select_wait_but_no_gated_wait() {
        for gated_backend in [false, true] {
            let (guest, kernel) = caught_sigchld_guest(gated_backend);
            let _proc = scripted_proc(&kernel);

            let gated = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTNOHAND);
            assert!(
                !gated.interrupted().unwrap(),
                "a pending caught SIGCHLD does not end a gated wait \
                 (gated_backend={gated_backend})"
            );

            let select = KernelSignalWait::for_select(&guest);
            assert!(
                select.interrupted().unwrap(),
                "the pending caught SIGCHLD ends a select wait (gated_backend={gated_backend})"
            );
            assert_eq!(
                kernel.lock().unwrap().pending,
                kernel_sigset_bit(libc::SIGCHLD),
                "the signal stays queued for the guest's handler"
            );
        }
    }

    /// A caught `SIGCHLD` the scheduler committed to the thread ends a gated
    /// wait, but only in the turn whose resume names it
    /// (`KernelSignalWait::admit_committed`), and never when a host-timed
    /// source a guest armed can post `SIGCHLD` too: that copy could arrive at a
    /// moment host timing sets (https://github.com/rrnewton/hermit/issues/4005).
    #[tokio::test]
    async fn a_committed_sigchld_ends_a_gated_wait_unless_a_host_timed_source_can_post_it() {
        let chld = kernel_sigset_bit(libc::SIGCHLD);
        let (guest, kernel) = caught_sigchld_guest(true);
        let _proc = scripted_proc(&kernel);

        let mut wait = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTNOHAND);
        assert!(
            !wait.interrupted().unwrap(),
            "an uncommitted SIGCHLD is held"
        );
        wait.admit_committed(chld);
        assert!(
            wait.interrupted().unwrap(),
            "a committed SIGCHLD ends the wait"
        );
        wait.admit_committed(0);
        assert!(
            !wait.interrupted().unwrap(),
            "the next turn's resume names nothing, so the SIGCHLD is held again"
        );

        let mut host_timed = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTNOHAND);
        host_timed.hold_until_return(chld);
        host_timed.admit_committed(chld);
        assert!(
            !host_timed.interrupted().unwrap(),
            "a SIGCHLD a host-timed source can post stays held although committed"
        );
    }

    /// The same `SIGCHLD` ends a select wait before an injection that is not a
    /// probe (`inject_absorbing`: the change of mask that blocks signals for the
    /// wait), with `ERESTARTNOHAND`, which the handler's run turns into `EINTR`.
    /// The waits of `select` and `pselect6` make every probe with
    /// `inject_first_probe`. A gated wait makes the injection, and the `SIGCHLD`
    /// that stops it is held until the call returns.
    #[tokio::test]
    async fn a_pending_caught_sigchld_ends_a_select_wait_before_the_injection() {
        let (mut guest, kernel) = caught_sigchld_guest(true);
        kernel.lock().unwrap().stops_for_pending = true;
        let _proc = scripted_proc(&kernel);

        let mut select = KernelSignalWait::for_select(&guest);
        let result = inject_mask_absorbing(&mut select, &mut guest, guest_mask()).await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND))),
            "{result:?}"
        );
        {
            let kernel = kernel.lock().unwrap();
            assert!(kernel.requested.is_empty(), "nothing is injected");
            assert_eq!(kernel.pending, kernel_sigset_bit(libc::SIGCHLD));
        }
        assert_eq!(select.held, None);

        let mut gated = KernelSignalWait::new(&guest, 0, false, Errno::ERESTARTNOHAND);
        let result = inject_mask_absorbing(&mut gated, &mut guest, guest_mask()).await;
        assert!(matches!(result, Ok(Ok(0))), "{result:?}");
        {
            let kernel = kernel.lock().unwrap();
            assert_eq!(kernel.taken, vec![libc::SIGCHLD]);
            assert_eq!(kernel.requested.len(), 2, "stopped once, then ran");
        }
        assert_eq!(gated.held.map(|held| held.kind), Some(HeldKind::Precious));
    }

    /// The kernel's next dequeue: the private queue before the shared one, a
    /// synchronous signal first, then the lowest number, outside the mask.
    #[test]
    fn next_dequeued_follows_the_kernel_order() {
        let bit = kernel_sigset_bit;
        let state = KernelSignalState {
            thread_pending: bit(libc::SIGUSR2) | bit(libc::SIGSEGV),
            shared_pending: bit(libc::SIGHUP),
            ..KernelSignalState::default()
        };
        assert_eq!(
            state.next_dequeued(0),
            Some((libc::SIGSEGV, SignalQueue::Thread))
        );
        assert_eq!(
            state.next_dequeued(bit(libc::SIGSEGV)),
            Some((libc::SIGUSR2, SignalQueue::Thread))
        );
        assert_eq!(
            state.next_dequeued(bit(libc::SIGSEGV) | bit(libc::SIGUSR2)),
            Some((libc::SIGHUP, SignalQueue::Shared))
        );
        assert_eq!(state.next_dequeued(!0), None);
    }

    /// A stop is identified only when exactly the predicted signal left its
    /// queue and the mask and dispositions are unchanged; arrivals do not matter.
    #[test]
    fn a_stop_is_identified_only_by_exactly_its_signal() {
        let bit = kernel_sigset_bit;
        let before = KernelSignalState {
            thread_pending: bit(libc::SIGUSR1) | bit(libc::SIGWINCH),
            shared_pending: bit(libc::SIGHUP),
            ..KernelSignalState::default()
        };
        let took = |after: KernelSignalState, signal, queue| {
            stop_took_only(&before, &after, signal, queue)
        };
        let after = KernelSignalState {
            thread_pending: bit(libc::SIGWINCH) | bit(libc::SIGUSR2),
            ..before
        };
        assert!(took(after, libc::SIGUSR1, SignalQueue::Thread));
        assert!(
            !took(after, libc::SIGWINCH, SignalQueue::Thread),
            "SIGWINCH is still queued"
        );
        let two_left = KernelSignalState {
            thread_pending: 0,
            ..before
        };
        assert!(!took(two_left, libc::SIGUSR1, SignalQueue::Thread));
        let shared_left = KernelSignalState {
            thread_pending: bit(libc::SIGWINCH),
            shared_pending: 0,
            ..before
        };
        assert!(!took(shared_left, libc::SIGUSR1, SignalQueue::Thread));
        let mask_changed = KernelSignalState {
            blocked: bit(libc::SIGUSR2),
            ..after
        };
        assert!(!took(mask_changed, libc::SIGUSR1, SignalQueue::Thread));
        let disposition_changed = KernelSignalState {
            ignored: bit(libc::SIGWINCH),
            ..after
        };
        assert!(!took(
            disposition_changed,
            libc::SIGUSR1,
            SignalQueue::Thread
        ));
    }

    /// A select wait with a pselect6 temporary mask (`with_wait_mask`) holds a
    /// caught signal that the temporary mask blocks, and the guest's own mask
    /// does not, when it stops an injection before `block` takes effect, as
    /// Linux keeps it pending for the whole call; without the temporary mask
    /// the same stop ends the wait, and a signal only the temporary mask
    /// unblocks ends it too (review of
    /// https://github.com/rrnewton/hermit/pull/4051).
    #[test]
    fn a_signal_the_temporary_mask_blocks_is_held_not_ended() {
        let bit = kernel_sigset_bit;
        let (guest, _kernel) = WaitGuest::live(guest_mask());
        let before = KernelSignalState {
            pending: bit(libc::SIGALRM),
            thread_pending: bit(libc::SIGALRM),
            blocked: guest_mask(),
            caught: bit(libc::SIGALRM) | bit(libc::SIGUSR2),
            ..KernelSignalState::default()
        };
        let plain = KernelSignalWait::for_select(&guest);
        assert_eq!(
            plain.stop_verdict(&before, libc::SIGALRM),
            StopVerdict::End(Errno::ERESTARTNOHAND)
        );
        let masked = KernelSignalWait::for_select(&guest).with_wait_mask(Some(bit(libc::SIGALRM)));
        assert_eq!(
            masked.stop_verdict(&before, libc::SIGALRM),
            StopVerdict::Absorb(HeldKind::Precious)
        );
        let unblocking = KernelSignalWait::for_select(&guest).with_wait_mask(Some(0));
        assert_eq!(
            unblocking.stop_verdict(&before, libc::SIGUSR2),
            StopVerdict::End(Errno::ERESTARTNOHAND)
        );
    }
}
