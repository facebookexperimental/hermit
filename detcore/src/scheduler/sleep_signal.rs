/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Cross-task signals that end an emulated `pause` or `nanosleep`
//! (<https://github.com/rrnewton/hermit/issues/3982>).
//!
//! Detcore never runs either call in the kernel. It parks the thread in the
//! scheduler's timed waiters on `SleepUntil(deadline)`, `INDEFINITE` for a
//! `pause`, and the thread stays in its ptrace stop there. A signal another
//! guest thread sends it is queued in the kernel and stays pending, because the
//! thread does not run, and nothing in the scheduler woke it: a child parked in
//! `pause()` that its parent killed with `SIGTERM` slept forever while the
//! parent sat in `wait4`.
//!
//! Only on a backend whose kernel reports the guest's signal state
//! (`models_signal_targets`), as for a precise-mode futex wait:
//!
//! - The sender's turn records the signal (`notify_signal_pending`) when the
//!   target's filed request is one of these sleeps
//!   ([`Scheduler::admits_sleep_signal`]). Not recorded: a `SIGKILL`, which the
//!   kernel acts on for a traced thread without the thread running, as before;
//!   a signal the target's waits hold (`futex_wait_held_signals`: `SIGCHLD` and
//!   host-timed signals), as for a precise futex wait, because the kernel can
//!   also post it at a host-chosen moment, so neither its pending bit nor a
//!   recorded send can say whether a later guest call took it back; and a
//!   signal Linux discards at generation because it is unblocked and ignored
//!   (`KernelSignalState::discarded_at_generation`), read in the sender's turn,
//!   before any later disposition change. Under ptrace the
//!   kernel queues that discarded signal anyway, for the tracer, and delivers
//!   it if a handler is installed before the thread resumes: a ptrace artifact
//!   this module does not change.
//! - The step2 drain, where no guest thread holds the turn, decides whether the
//!   signal still interrupts the sleep ([`Scheduler::sleep_interrupting_signals`]):
//!   it must still be pending for the target, so a stop signal a `SIGCONT`
//!   cancelled, or one that `sigaction(SIG_IGN)` flushed, does not count; and
//!   unblocked by the mask the target stopped with, and caught or acted on by
//!   default under the dispositions the kernel holds now. A disposition changes
//!   only in some thread's turn, and a recorded signal's pending bit only by a
//!   guest's own call, since held signals are never recorded.
//! - An interrupting signal replaces the request with the one-resource signal
//!   set (`WaitidSignals`) and queues the thread at the front of its band, so it
//!   is the next thread to resume and dequeue the signal. Its turn answers
//!   `Signaled`.
//! - A state that cannot be read leaves the sleep parked: when the thread is
//!   gone, its retirement removes it; otherwise the run ends with a diagnostic
//!   rather than a guest interruption invented from a failed read.
//!
//! Membership in the drain is fixed by the sender's committed turn, and the
//! decision reads only scheduler-ordered state, so where the wake falls in the
//! schedule does not depend on host timing.
//!
//! The woken thread decides again in its own turn (`syscalls::signal::sleep_signal`),
//! and also checks before it parks, so a signal sent while it was stopped short
//! of filing its request is not lost. `pause` returns `ERESTARTNOHAND` for a
//! woken signal still pending for it, so the kernel runs a handler and returns
//! `EINTR`, kills the process, or stops it and restarts the call; `EINTR` for a
//! held one the scheduler sent (a child-exit `SIGCHLD`); and with none it waits
//! again (`Detcore::handle_pause`). A
//! `nanosleep` returns `EINTR` with the remaining time, or sleeps again until
//! its original deadline. Its restart after a stop would have to keep the deadline, which
//! Detcore does not model for it, so a stop signal does not end a `nanosleep`;
//! the stop takes effect when the sleep ends, as before. Nor does a signal that
//! arrives once the deadline has passed: the sleep completes normally and the
//! signal is delivered as the call returns.

use super::Scheduler;
use super::kernel_signal_bit;
use crate::resources::NANOSLEEP_FYI;
use crate::resources::PAUSE_FYI;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::KernelSignalState;
use crate::syscalls::read_thread_signal_state;
use crate::syscalls::thread_is_gone;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::SigWrapper;

/// An emulated sleep that a cross-task signal can end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SignalSleep {
    /// `pause`: `SleepUntil(INDEFINITE)` tagged [`PAUSE_FYI`].
    Pause,
    /// `nanosleep` or `clock_nanosleep`: `SleepUntil(deadline)` tagged [`NANOSLEEP_FYI`].
    Nanosleep { deadline: LogicalTime },
}

/// Why a sleeper's kernel signal state could not be read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SleeperStateUnreadable {
    /// The thread no longer exists.
    Gone,
    /// The thread exists, but its `/proc` status could not be read or parsed.
    Unreadable(reverie::Errno),
}

impl Scheduler {
    /// The sleep `dettid`'s filed request is, if it is one a cross-task signal
    /// can end on this backend.
    pub(super) fn signal_sleep(&self, dettid: DetTid) -> Option<SignalSleep> {
        if !self.models_signal_targets {
            return None;
        }
        let resources = self.next_turns.get(&dettid)?.req.try_read()?.ok()?;
        if resources.resources.len() != 1 {
            return None;
        }
        let Some((ResourceID::SleepUntil(deadline), _)) = resources.resources.iter().next() else {
            return None;
        };
        match resources.fyi.as_str() {
            PAUSE_FYI if deadline.is_indefinite() => Some(SignalSleep::Pause),
            NANOSLEEP_FYI => Some(SignalSleep::Nanosleep {
                deadline: *deadline,
            }),
            _ => None,
        }
    }

    /// `SIGCHLD`, if a guest's thread-directed copy of it, recorded in the
    /// sender's committed turn, is in `dettid`'s private queue
    /// (`ChildExitSigchldLedger::private_sent`) and no host-timed source can
    /// post `SIGCHLD` to it. Such a copy is not held: it is recorded for a
    /// sleep and decides like any other signal, from the record, not from
    /// `/proc` (<https://github.com/rrnewton/hermit/issues/4132>).
    pub(super) fn private_sigchld(&self, dettid: DetTid) -> u64 {
        if self.child_exit_sigchld.private_sent.contains(&dettid) {
            kernel_signal_bit(libc::SIGCHLD) & !self.host_timed_signals(dettid)
        } else {
            0
        }
    }

    /// Whether a cross-task `signal`, just sent to `dettid` in the sender's
    /// turn, is recorded for the drain because it may end the sleep its request
    /// is. A state that cannot be read here is recorded, and the drain decides.
    pub(super) fn admits_sleep_signal(&self, dettid: DetTid, signal: SigWrapper) -> bool {
        let bit = kernel_signal_bit(signal.raw());
        if signal.raw() == libc::SIGKILL
            || self.signal_sleep(dettid).is_none()
            || self.futex_wait_held_signals(dettid) & !self.private_sigchld(dettid) & bit != 0
        {
            return false;
        }
        match self.read_sleeper_signal_state(dettid) {
            Ok(state) => state.discarded_at_generation() & bit == 0,
            Err(_) => true,
        }
    }

    /// The signals that end `dettid`'s `sleep` now: pending for it and not held
    /// by its waits (`futex_wait_held_signals`), unblocked by its mask, and
    /// caught or acted on by default under the dispositions the kernel holds
    /// now. A default stop ends a `pause` but not a `nanosleep`.
    ///
    /// ⚠️ CALL THIS ONLY AT THE STEP2 DRAIN, where no guest thread holds the
    /// turn; see `parked_futex_interrupting_signals` for why the kernel's
    /// dispositions are then a function of the schedule, and for the threads
    /// outside the scheduler that are the exception.
    pub(super) fn sleep_interrupting_signals(
        &self,
        dettid: DetTid,
        sleep: SignalSleep,
    ) -> Result<u64, SleeperStateUnreadable> {
        let state = self.read_sleeper_signal_state(dettid)?;
        let interrupting = (state.pending & !self.futex_wait_held_signals(dettid)
            | self.private_sigchld(dettid))
            & state.interrupting(state.blocked);
        Ok(match sleep {
            SignalSleep::Pause => interrupting,
            SignalSleep::Nanosleep { .. } => {
                interrupting
                    & !(state.default_job_control_stops() | kernel_signal_bit(libc::SIGSTOP))
            }
        })
    }

    pub(super) fn read_sleeper_signal_state(
        &self,
        dettid: DetTid,
    ) -> Result<KernelSignalState, SleeperStateUnreadable> {
        #[cfg(test)]
        if let Some(state) = self.test_kernel_signal_states.get(&dettid) {
            return Ok(*state);
        }
        #[cfg(test)]
        if let Some(failure) = self.test_sleeper_read_failures.get(&dettid) {
            return Err(*failure);
        }
        let process = reverie::Pid::from_raw(self.sigchld_process(dettid).as_raw());
        let thread = reverie::Pid::from_raw(dettid.as_raw());
        read_thread_signal_state(process, thread).map_err(|errno| {
            if thread_is_gone(process, thread) {
                SleeperStateUnreadable::Gone
            } else {
                SleeperStateUnreadable::Unreadable(errno)
            }
        })
    }

    /// Applies cross-task `signals` drained for `dettid`, whose request is
    /// `sleep`: ends the sleep for those that interrupt it, and otherwise leaves
    /// it as it is.
    pub(super) fn drain_sleep_signals(
        &mut self,
        dettid: DetTid,
        sleep: SignalSleep,
        mut signals: Vec<SigWrapper>,
    ) {
        if let SignalSleep::Nanosleep { deadline } = sleep
            && deadline <= self.committed_time
        {
            // The deadline has passed: the sleep completes at its timed pop, or
            // at its admission if it never blocked, and the signal is delivered
            // as the call returns, as Linux does after the timer has fired.
            return;
        }
        let interrupting = match self.sleep_interrupting_signals(dettid, sleep) {
            Ok(interrupting) => interrupting,
            Err(SleeperStateUnreadable::Gone) => {
                // Its retirement takes it out of the timed waiters.
                tracing::debug!(
                    "[dtid {}] is gone; its {:?} is left to its retirement.",
                    dettid,
                    sleep
                );
                return;
            }
            Err(SleeperStateUnreadable::Unreadable(errno)) => {
                self.terminal_deadlock.get_or_insert_with(|| {
                    format!(
                        "HERMIT_DEADLOCK: cannot read the signal state of dettid {} ({}) to decide whether cross-task signals {:?} end its {:?}",
                        dettid, errno, signals, sleep
                    )
                });
                return;
            }
        };
        signals.retain(|signal| interrupting & kernel_signal_bit(signal.raw()) != 0);
        signals.sort_by_key(SigWrapper::raw);
        signals.dedup();
        if signals.is_empty() {
            tracing::debug!(
                "[dtid {}] no drained signal interrupts its {:?}; leaving it parked.",
                dettid,
                sleep
            );
            return;
        }
        tracing::info!(
            "[dtid {}] cross-task signals {:?} end its {:?}.",
            dettid,
            signals,
            sleep
        );
        // A sleep filed but not yet admitted is still queued; one admitted is in
        // the timed waiters, which `force_unblock_thread_at` clears.
        if self.run_queue.contains_tid(dettid) {
            let removed = self.run_queue.remove_tid(dettid);
            debug_assert!(
                removed,
                "run_queue.contains_tid disagreed with remove_tid for {dettid}"
            );
        }
        let mut resources = Resources::new(dettid);
        resources.insert(ResourceID::WaitidSignals(signals), Permission::W);
        self.force_unblock_thread_at(dettid, resources, true);
    }
}
