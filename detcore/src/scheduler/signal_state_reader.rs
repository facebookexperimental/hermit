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

use reverie::syscalls::Errno;

use super::Scheduler;
use crate::syscalls::KernelSignalState;
use crate::syscalls::reads_signal_state_from_backend;
use crate::syscalls::signal_sets_errno;
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
