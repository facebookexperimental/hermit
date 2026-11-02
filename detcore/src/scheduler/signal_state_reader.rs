/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The scheduler's reader of a guest thread's signal state on a backend whose
//! guest threads are not host tasks (SIGSHARE S2; design
//! `ai_docs/2026-10-29-kvm-sigshare-design.md`, sections 7.2 and 10).
//!
//! Wherever guest threads are host tasks (ptrace, LiteInst, SaBRe, DBT) the
//! scheduler reads a thread's mask and dispositions from host `/proc`, as it
//! always has, and this reader answers nothing. Where the backend owns signal
//! delivery because they are not (`provides_process_signal_control`, KVM),
//! host `/proc/<pid>/task/<tid>` names an unrelated host task, if any, so the
//! reader asks the backend's run-scoped signal control instead
//! (`ProcessSignalControl::signal_sets`), for the exact task lifetime the
//! scheduler bound at the thread's start (`real_timers`).
//!
//! The answer describes the moment of the call, so a caller asks only at a
//! committed point where no guest thread holds the turn (the step-2 drain),
//! as it reads `/proc` there on ptrace.
//!
//! Beside the reader, #4929 slice 2 keeps here what its timer decisions read
//! (<https://github.com/rrnewton/hermit/issues/4929>): with a control
//! installed, the temporary mask Linux holds installed for a thread, which the
//! backend's `blocked` set does not hold ([`Scheduler::controlled_temporary_mask`]),
//! the mask a decision reads ([`Scheduler::read_guest_signal_mask`]), and
//! whether a thread's state is settled enough to read
//! ([`Scheduler::reads_settled_signal_state`]). Every read happens where no
//! guest thread runs: a timer expiry and a choice of receiver are made in the
//! scheduler's maintenance, on a quiesced state, so the answer is a function
//! of the schedule. An exiting task's retirement and an exec are ruled out
//! from the scheduler's committed state before a read. An outstanding
//! delivery permit cannot change the state at these points: a quiesced point
//! waits for every thread's request, and a permit holder files its request
//! only from its delivery hook, which the backend runs after it has dequeued
//! the signal; the handler's mask is installed after that hook returns, in
//! the holder's own `InboundSignal` turn, which the schedule orders after the
//! read.

use reverie::syscalls::Errno;

use super::Scheduler;
use crate::syscalls::KernelSignalState;
use crate::syscalls::read_kernel_signal_state;
use crate::syscalls::read_thread_signal_state;
use crate::syscalls::reads_signal_state_from_backend;
use crate::syscalls::signal_sets_errno;
use crate::syscalls::thread_is_gone;
use crate::types::DetTid;

impl Scheduler {
    /// `dettid`'s signal state as its backend reports it, where Detcore reads
    /// it from the backend (`reads_signal_state_from_backend`), and `None`
    /// on every other backend, whose callers keep their `/proc` read.
    ///
    /// Where it answers, the errors are:
    /// - `ESRCH`: the backend names no live task of this run for the lifetime
    ///   bound at the thread's start, or no lifetime is bound for `dettid`;
    /// - `ENOSYS`: no signal control is installed (a run without
    ///   sequentialized threads), or the backend does not report the fact;
    /// - `EAGAIN`: an exec of the task's process overlapped the read twice in
    ///   a row (one overlap is asked again, as the method documents);
    /// - any other errno the backend gives.
    ///
    /// Callers treat every error as an unknown state, which is what the
    /// refused `/proc` read gave them before.
    pub(crate) fn backend_signal_state(
        &self,
        dettid: DetTid,
    ) -> Option<Result<KernelSignalState, Errno>> {
        reads_signal_state_from_backend(&self.backend)
            .then(|| self.read_backend_signal_state(dettid))
    }

    /// Guest thread `dettid`'s signal state, wherever Detcore reads it
    /// (SIGSHARE S3a, design section 7.2): from the backend where it reads it
    /// there ([`Self::backend_signal_state`]), and otherwise from host
    /// `/proc/<pid>/task/<tid>/status` ([`read_kernel_signal_state`]), as
    /// before. `pid` and `tid` name the host task that a `/proc` read opens,
    /// as each caller named it before; the backend's answer depends on
    /// neither, because the lifetime bound at the thread's start names the
    /// thread exactly.
    ///
    /// Every scheduler reader of a guest thread's signal state goes through
    /// this method or [`Self::guest_thread_signal_state`], so no reader asks
    /// host `/proc` about a task that is not the guest thread.
    pub(crate) fn guest_signal_state(
        &self,
        dettid: DetTid,
        pid: i32,
        tid: i32,
    ) -> Result<KernelSignalState, Errno> {
        self.backend_signal_state(dettid).unwrap_or_else(|| {
            read_kernel_signal_state(reverie::Pid::from_raw(pid), reverie::Pid::from_raw(tid))
        })
    }

    /// [`Self::guest_signal_state`] for a thread whose `pid` may not be its
    /// thread group, as [`read_thread_signal_state`] reads it on a backend
    /// that reads `/proc`.
    pub(crate) fn guest_thread_signal_state(
        &self,
        dettid: DetTid,
        pid: i32,
        tid: i32,
    ) -> Result<KernelSignalState, Errno> {
        self.backend_signal_state(dettid).unwrap_or_else(|| {
            read_thread_signal_state(reverie::Pid::from_raw(pid), reverie::Pid::from_raw(tid))
        })
    }

    /// Whether guest thread `dettid` no longer exists: where Detcore reads
    /// signal state from the backend, whether the backend names no live task
    /// for the lifetime bound at its start (`ESRCH`, which includes a thread
    /// with no bound lifetime); elsewhere the host probe [`thread_is_gone`],
    /// as before. On a backend whose guest threads are not host tasks a host
    /// probe of the same number would describe an unrelated host task.
    pub(crate) fn guest_thread_is_gone(&self, dettid: DetTid, pid: i32, tid: i32) -> bool {
        match self.backend_signal_state(dettid) {
            Some(read) => read == Err(Errno::ESRCH),
            None => thread_is_gone(reverie::Pid::from_raw(pid), reverie::Pid::from_raw(tid)),
        }
    }

    fn read_backend_signal_state(&self, dettid: DetTid) -> Result<KernelSignalState, Errno> {
        let control = self.parked.control.as_ref().ok_or(Errno::ENOSYS)?;
        let pid = self.registered_process(dettid).ok_or(Errno::ESRCH)?;
        let (_, task) = self
            .real_timers
            .task_identity(pid, dettid)
            .ok_or(Errno::ESRCH)?;
        let read = || control.process.signal_sets(task).map_err(signal_sets_errno);
        let sets = match read() {
            // The task's process replaced its image while the sets were read;
            // asking again reads the new image (`signal_sets`).
            Err(Errno::EAGAIN) => read(),
            answer => answer,
        }?;
        Ok(KernelSignalState::from_backend_sets(&sets))
    }
}

/// A thread's temporary mask is unknown (`Scheduler::controlled_temporary_mask`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TemporaryMaskUnknown;

impl Scheduler {
    /// Whether the reader takes a guest thread's state from the installed
    /// process signal control (module docs), rather than from the host's
    /// `/proc`.
    pub(crate) fn reads_signal_state_through_control(&self) -> bool {
        self.signal_control_installed()
    }

    /// `thread`'s signal state, from the one reader
    /// ([`Self::guest_signal_state`]): the backend's sets where Detcore reads
    /// them there, and otherwise host `/proc` of the thread in its process.
    /// Unit tests on a backend that reads `/proc` script a thread's state
    /// (`test_kernel_signal_states`); an unscripted one reads through
    /// `read_kernel_signal_state`'s own test seam.
    pub(crate) fn read_guest_signal_state(
        &self,
        thread: DetTid,
    ) -> Result<KernelSignalState, Errno> {
        #[cfg(test)]
        if !reads_signal_state_from_backend(&self.backend)
            && let Some(state) = self.test_kernel_signal_states.get(&thread)
        {
            return Ok(*state);
        }
        let process = self.thread_tree.process_of(thread).unwrap_or(thread);
        self.guest_signal_state(thread, process.as_raw(), thread.as_raw())
    }

    /// `thread`'s mask as Linux's `blocked` holds it now, from the reader's
    /// source, for a thread stopped at a request: the kernel's `SigBlk`
    /// without a control (`read_thread_blocked_mask`, as before); with one,
    /// the temporary mask Linux holds installed for it where there is one
    /// (`controlled_temporary_mask`), and otherwise the control's `blocked`
    /// set. `None` when it cannot be read and when the temporary mask is
    /// unknown.
    pub(crate) fn read_guest_signal_mask(&self, thread: DetTid) -> Option<u64> {
        if !self.reads_signal_state_through_control() {
            return self.read_thread_blocked_mask(thread);
        }
        match self.controlled_temporary_mask(thread) {
            Ok(Some(mask)) => Some(mask),
            Ok(None) => self
                .read_guest_signal_state(thread)
                .ok()
                .map(|state| state.blocked),
            Err(TemporaryMaskUnknown) => None,
        }
    }

    /// With a control installed, the temporary mask Linux holds installed for
    /// `thread` now, in place of its own (`set_user_sigmask`,
    /// kernel/signal.c:3272-3288 at v6.17), which the control's `blocked` set
    /// does not hold: `Ok(None)` where the thread's own mask applies.
    ///
    /// Read from the scheduler's committed state, or from a request the
    /// thread filed in its own turn, never from one filed at a moment host
    /// timing chooses (<https://github.com/rrnewton/hermit/issues/4929>):
    /// - a thread running a call outside the schedule (`external_io_blockers`,
    ///   `rt_sigsuspend_blockers`): the call's mask, recorded when the
    ///   scheduler released it (`controlled_background_masks`), or else its
    ///   own. Its request slot is empty while the call runs and then holds its
    ///   rejoin, filed when host timing finishes the call, so the slot is not
    ///   read. Its own mask cannot change meanwhile: a delivery needs a permit,
    ///   which only the holder of the serial grant gets
    ///   (`authorize_signal_boundary`);
    /// - a thread readmitted with a rejoin request: the mask its readmission
    ///   recorded (`BlockedPool::continuation_masks`), or else its own;
    /// - any other thread: its filed request's temporary mask
    ///   (`Resources::blocked_signal_mask`, which a controlled backend's
    ///   probed call fills too), or else its own. `Err` where the request says
    ///   the mask is unknown.
    pub(crate) fn controlled_temporary_mask(
        &self,
        thread: DetTid,
    ) -> Result<Option<u64>, TemporaryMaskUnknown> {
        let known = |mask: Option<u64>| mask.map(Some).ok_or(TemporaryMaskUnknown);
        if self.blocked.external_io_blockers.contains_key(&thread)
            || self.blocked.rt_sigsuspend_blockers.contains_key(&thread)
        {
            return self
                .blocked
                .controlled_background_masks
                .get(&thread)
                .map_or(Ok(None), |mask| known(*mask));
        }
        let Some(Ok(request)) = self.next_turns.get(&thread).and_then(|t| t.req.try_read()) else {
            return Ok(None);
        };
        if request.is_rejoin() {
            return self
                .blocked
                .continuation_masks
                .get(&thread)
                .map_or(Ok(None), |mask| known(*mask));
        }
        if request.blocked_signal_mask_unknown {
            return Err(TemporaryMaskUnknown);
        }
        Ok(request.blocked_signal_mask)
    }

    /// `request` as the scheduler's polling record prints it (the NONCOMMIT
    /// SKIP line): as it is, except that with a control installed, a polled
    /// call's temporary mask (`Resources::blocked_signal_mask` on a lone
    /// `InternalIOPolling` resource, which the probed call files for this
    /// reader, `handle_internal_pselect6`) is left out. The traced backend
    /// hands the scheduler the same fact outside the request
    /// (`publish_wait_mask`), so printing it made that record alone differ
    /// from ptrace's in the canonical INFO stream, which lowered the
    /// cross-backend parity credit of every cell with a masked `pselect6`
    /// (<https://github.com/rrnewton/hermit/issues/4929>, review round 1).
    /// The request itself, and every decision that reads it, keep the mask.
    pub(crate) fn logged_polling_request<'a>(
        &self,
        request: &'a crate::resources::Resources,
    ) -> std::borrow::Cow<'a, crate::resources::Resources> {
        if self.reads_signal_state_through_control()
            && request.blocked_signal_mask.is_some()
            && request.resources.len() == 1
            && request
                .resources
                .contains_key(&crate::resources::ResourceID::InternalIOPolling)
        {
            let mut printed = request.clone();
            printed.blocked_signal_mask = None;
            std::borrow::Cow::Owned(printed)
        } else {
            std::borrow::Cow::Borrowed(request)
        }
    }

    /// Whether `thread` is in a state whose signal state the control answers
    /// stably now (module docs): it is registered, its exit has not begun (no
    /// exit fence, no logical kill, no filed exit request, no pending removal),
    /// and no exec is tearing its process down. Read from the scheduler's
    /// committed state alone, so a thread whose exit or exec is in flight is
    /// treated as unreadable whatever host timing has done to it, rather than
    /// read as gone or as changed at a moment host timing chooses.
    pub(crate) fn reads_settled_signal_state(&self, thread: DetTid) -> bool {
        let Some(process) = self.registered_process(thread) else {
            return false;
        };
        self.next_turns.contains_key(&thread)
            && !self.parked.exit_fences.contains_key(&thread)
            && !self.thread_is_logically_killed(thread)
            && !self.pending_run_queue_removals.contains_key(&thread)
            && !self.exec_teardowns.contains_key(&process)
            && !self.next_turns.get(&thread).is_some_and(|turn| {
                turn.req.try_read().is_some_and(|request| {
                    request.is_ok_and(|request| {
                        request
                            .resources
                            .keys()
                            .any(|rid| matches!(rid, crate::resources::ResourceID::Exit { .. }))
                    })
                })
            })
    }
}
