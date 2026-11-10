/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The scheduler's SIGALRM ledger, for guest SIGALRM handlers on a backend
//! that runs Detcore inside each guest process (in-guest LiteInst).
//!
//! Design: dev-hermit `ai_docs/transient/liteinst-inguest-signal-handlers-design-20261007.md`,
//! phase 1, step I1a, under <https://github.com/rrnewton/hermit/issues/3520>.
//!
//! - A process's runtime publishes whether its SIGALRM disposition is a guest
//!   handler. While it is, an emulated `alarm`/`setitimer` expiry adds a ledger
//!   entry instead of sending a physical signal. A second expiry while one is
//!   pending is absorbed: standard-signal coalescing in the process's one
//!   shared queue.
//! - The runtime also publishes each thread's "SIGALRM virtually blocked" bit,
//!   inside the turn of the call that changes its mask. No expiry commits
//!   while a thread holds its turn, so the bit is current at every expiry and
//!   every admission. An entry is *due* when that bit is clear. (Publishing it
//!   by control message rather than on every request leaves the request wire,
//!   `Resources`, unchanged.)
//! - A due entry ends an emulated `pause`: at the pause's own admission, when
//!   the entry is already due as it is selected, or at the expiry, when the
//!   pause is already parked. Either way the pause returns EINTR.
//! - A due entry whose owner is blocked in any other wait records a
//!   determinism loss: Linux would interrupt that wait, and phase 1 cannot.
//!   That includes a polling wait (`InternalIOPolling` retried after EAGAIN),
//!   whose thread stays on the run queue between retries. A running thread, or
//!   one whose filed request has not blocked, takes the entry at its syscall's
//!   completion; that delivery is the runtime's (later steps).
//! - Every change happens at a scheduler commit: an expiry, an admission, or
//!   the sweep that opens step 2. So what is pending, and what is due, is a
//!   function of the schedule.
//!
//! Nothing publishes a handled SIGALRM until the runtime admits guest
//! handlers, so this module changes no run today.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use nix::sys::signal::Signal;
use tracing::info;

use super::Scheduler;
use super::ThreadStatus;
use crate::resources::PAUSE_FYI;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::tool_global::SigalrmControl;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::SigWrapper;

/// The scheduler's record of guest-handled SIGALRMs.
#[derive(Debug, Default)]
pub(crate) struct SigalrmLedger {
    /// Processes whose SIGALRM disposition is a guest handler.
    handled: BTreeSet<DetPid>,
    /// Processes with a pending SIGALRM: at most one each.
    pending: BTreeSet<DetPid>,
    /// Each thread's "SIGALRM virtually blocked" bit, as its runtime last
    /// published it.
    blocked: BTreeMap<DetTid, bool>,
    /// Threads whose `InternalIOPolling` retry the run queue has deferred as a
    /// poller. Kept here because the deferral resets the stored request's
    /// attempt count (`upgrade_polled_to_runnable`); cleared when the request
    /// is granted (`clear_nextturn`) or substituted (`force_unblock_thread`).
    polling: BTreeSet<DetTid>,
    /// Pending entries whose determinism loss is already recorded, so a
    /// stranded entry is reported once.
    lost: BTreeSet<DetPid>,
    /// Whether any guest armed a kernel producer of SIGALRM
    /// (`SigalrmControl::ArmProducer`). Never cleared: the producer may fire
    /// at any later time.
    producer_armed: bool,
}

impl Scheduler {
    /// Whether `process` has published a guest handler for SIGALRM.
    // Called by the runtime's control messages, which arrive with its handler
    // admission (phase 1 step I3); until then only tests call it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn sigalrm_handled(&self, process: DetPid) -> bool {
        self.sigalrm.handled.contains(&process)
    }

    /// Whether `process` has a pending SIGALRM entry.
    // Called by the runtime's control messages, which arrive with its handler
    // admission (phase 1 step I3); until then only tests call it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn sigalrm_pending(&self, process: DetPid) -> bool {
        self.sigalrm.pending.contains(&process)
    }

    /// Records `process`'s SIGALRM disposition, published by its runtime inside
    /// the turn of the call that changed it. A handler that becomes ignored or
    /// default discards a pending entry, as Linux discards a pending signal that
    /// becomes ignored. The runtime refuses the change to default while an
    /// entry is pending, so a default-acted signal is never discarded here.
    pub(crate) fn set_sigalrm_handled(&mut self, process: DetPid, handled: bool) {
        if handled {
            self.sigalrm.handled.insert(process);
        } else {
            self.sigalrm.handled.remove(&process);
            self.sigalrm.pending.remove(&process);
            self.sigalrm.lost.remove(&process);
        }
    }

    /// Decides a SIGALRM control message from `thread`'s Detcore, sent in its
    /// turn before the call reaches the kernel; returns whether it is refused.
    ///
    /// While no process handles SIGALRM, nothing is refused, so runs without a
    /// guest handler behave as before; an allowed producer arming is recorded.
    /// Otherwise a sender that does not hold the serial grant is refused: its
    /// question could race a commit. A send is refused when it reaches a
    /// handling process (one the caller cannot name could be one), every
    /// producer arming is refused, and a recurring `ITIMER_REAL` is refused in
    /// a handling process.
    pub(crate) fn sigalrm_control(&mut self, thread: DetTid, control: SigalrmControl) -> bool {
        // The runtime's publications (phase 1 step I3) record what its
        // runtime already changed inside the caller's turn; they hold whether
        // or not any process handles SIGALRM yet.
        let process = self.sigchld_process(thread);
        match control {
            SigalrmControl::Publish { handled, blocked } => {
                self.set_sigalrm_handled(process, handled);
                self.set_sigalrm_blocked(thread, blocked);
                return false;
            }
            SigalrmControl::PublishBlocked(blocked) => {
                self.set_sigalrm_blocked(thread, blocked);
                return false;
            }
            SigalrmControl::DeliveryLost => {
                crate::detlog::write_loss_notice(&format!(
                    "thread {thread} of process {process} took a pending guest-handled SIGALRM, but the runtime could not deliver it"
                ));
                self.sigalrm.lost.insert(process);
                return false;
            }
            SigalrmControl::EntryDeliveryUnfaithful => {
                crate::detlog::write_loss_notice(&format!(
                    "thread {thread} of process {process} delivers a guest-handled SIGALRM before a syscall that has not run; its handler cannot see the registers Linux would show it (rcx and r11 as the syscall instruction left them)"
                ));
                self.sigalrm.lost.insert(process);
                return false;
            }
            _ => {}
        }
        // Its questions, like every other control, are refused to a caller
        // without the serial grant while any process handles SIGALRM: the
        // answer could race a commit. (A handling process is sequentialized,
        // so its own calls hold the grant.)
        if !self.sigalrm.handled.is_empty() && !self.holds_serial_grant(thread) {
            return true;
        }
        match control {
            SigalrmControl::InstallHandler => {
                // Also refused while the caller's process has a recurring
                // ITIMER_REAL armed: phase 1 does not model one in a handling
                // process (it is refused once a handler exists, too).
                let recurring = self
                    .blocked
                    .timed_waiters
                    .alarm_state(process)
                    .is_some_and(|(_, interval)| interval != crate::types::LogicalTime::ZERO);
                let refused = self.sigalrm_producer_armed() || recurring;
                if refused {
                    info!(
                        "[dtid {}] SIGALRM handler refused: a guest armed a kernel producer of SIGALRM, or its process has a recurring ITIMER_REAL (signal phase 1).",
                        thread
                    );
                }
                return refused;
            }
            SigalrmControl::HandlerToDefault | SigalrmControl::PendingEntry => {
                return self.sigalrm.pending.contains(&process);
            }
            SigalrmControl::DueEntry => return self.sigalrm_due(thread),
            SigalrmControl::TakeEntry => {
                let taken = self.take_sigalrm(process, thread);
                if taken {
                    info!(
                        "[dtid {}] takes its process's pending SIGALRM for delivery to its handler.",
                        thread
                    );
                }
                return taken;
            }
            _ => {}
        }
        if self.sigalrm.handled.is_empty() {
            if control == SigalrmControl::ArmProducer {
                self.sigalrm.producer_armed = true;
            }
            return false;
        }
        if !self.holds_serial_grant(thread) {
            return true;
        }
        if control == SigalrmControl::UnadmittedStdioIo {
            if self.sigalrm_due(thread) {
                self.record_sigalrm_loss(
                    DetPid::from_raw(thread.as_raw()),
                    thread,
                    "a due SIGALRM found it entering I/O on inherited stdio that may sleep unadmitted",
                );
            }
            return false;
        }
        let refused = match control {
            SigalrmControl::SendTo(target) => {
                self.sigalrm.handled.contains(&self.sigchld_process(target))
            }
            SigalrmControl::SendToUnnamed | SigalrmControl::ArmProducer => true,
            SigalrmControl::UnadmittedStdioIo
            | SigalrmControl::InstallHandler
            | SigalrmControl::HandlerToDefault
            | SigalrmControl::PendingEntry
            | SigalrmControl::Publish { .. }
            | SigalrmControl::PublishBlocked(_)
            | SigalrmControl::DueEntry
            | SigalrmControl::TakeEntry
            | SigalrmControl::DeliveryLost
            | SigalrmControl::EntryDeliveryUnfaithful => unreachable!("answered above"),
            SigalrmControl::ArmRecurringTimer => {
                self.sigalrm.handled.contains(&self.sigchld_process(thread))
            }
        };
        if refused {
            info!(
                "[dtid {}] {:?} refused: a guest handles SIGALRM (signal phase 1).",
                thread, control
            );
        }
        refused
    }

    /// Whether any guest armed a kernel producer of SIGALRM; a handler
    /// installation is refused once one has.
    pub(crate) fn sigalrm_producer_armed(&self) -> bool {
        self.sigalrm.producer_armed
    }

    /// Records `thread`'s "SIGALRM virtually blocked" bit, published by its
    /// runtime inside the turn of the call that changed its mask (and at
    /// handler admission).
    pub(crate) fn set_sigalrm_blocked(&mut self, thread: DetTid, blocked: bool) {
        self.sigalrm.blocked.insert(thread, blocked);
    }

    /// Notes that step 4 deferred `thread`'s polling retry `request` as a
    /// poller, before the deferral resets its attempt count.
    pub(crate) fn note_sigalrm_polling_wait(&mut self, thread: DetTid, request: &Resources) {
        if Self::is_polling_retry(request) {
            self.sigalrm.polling.insert(thread);
        }
    }

    /// Forgets that `thread` is in a polling wait: its request was granted or
    /// substituted.
    pub(crate) fn end_sigalrm_polling_wait(&mut self, thread: DetTid) {
        self.sigalrm.polling.remove(&thread);
    }

    /// A retry of an `InternalIOPolling` operation after EAGAIN.
    fn is_polling_retry(request: &Resources) -> bool {
        request.poll_attempt > 0
            && request
                .resources
                .contains_key(&ResourceID::InternalIOPolling)
    }

    /// Whether `thread` is in a polling wait: its filed request is a retry
    /// step 4 has not seen yet, or step 4 deferred it as a poller. Between its
    /// grant and its next request the thread holds its turn, when no expiry
    /// commits.
    pub(super) fn in_polling_wait(&self, thread: DetTid) -> bool {
        self.sigalrm.polling.contains(&thread)
            || self
                .next_turns
                .get(&thread)
                .and_then(|turn| turn.req.try_read())
                .is_some_and(|request| request.is_ok_and(|r| Self::is_polling_retry(&r)))
    }

    /// Diverts an expiry of `signal` for `process` into the ledger when the
    /// process handles SIGALRM. Returns whether it was diverted; otherwise the
    /// caller sends the signal as before.
    pub(crate) fn divert_sigalrm(&mut self, process: DetPid, signal: Signal) -> bool {
        if signal != Signal::SIGALRM || !self.sigalrm.handled.contains(&process) {
            return false;
        }
        if self.sigalrm.pending.insert(process) {
            info!(
                "[dpid {}] SIGALRM expired; a guest handler is installed, so it is pending in the ledger.",
                process
            );
        } else {
            info!(
                "[dpid {}] SIGALRM expired while one was already pending; coalesced.",
                process
            );
        }
        self.act_on_pending_sigalrm(process);
        true
    }

    /// The sweep at the start of step 2, after queued plain sleeps are
    /// registered (`Scheduler::step2_register_queued_sleeps`): acts on every
    /// pending entry, in process order. It records the loss of a thread that
    /// blocked in a wait admitted with its entry already due, which, under
    /// first-pass registration, includes a plain sleep filed in the last pass. It runs before the timed
    /// events, so a sleep that blocked and is now expiring is seen blocked.
    pub(crate) fn step2_sigalrm_ledger(&mut self) {
        let pending: Vec<DetPid> = self.sigalrm.pending.iter().copied().collect();
        for process in pending {
            self.act_on_pending_sigalrm(process);
        }
    }

    /// Admission of a `SleepUntil(INDEFINITE)` request by the selected `thread`:
    /// when it is an emulated `pause` and its process's entry is due, the
    /// request is resolved now with `Signaled(SIGALRM)` instead of parking, in
    /// the same selected turn. Returns whether it was; the caller then grants
    /// the turn.
    pub(crate) fn admit_pause_with_due_sigalrm(&mut self, thread: DetTid) -> bool {
        if !self.sigalrm_due(thread) || !self.filed_pause(thread) {
            return false;
        }
        info!(
            "[dtid {}] pause admitted with a due SIGALRM; it returns at once.",
            thread
        );
        let wake = Self::sigalrm_wake(thread);
        self.substitute_request(thread, wake)
    }

    /// Takes `process`'s entry for delivery, if it has one and it is due for
    /// `thread`. The caller has checked that `thread` holds the serial grant.
    pub(crate) fn take_sigalrm(&mut self, process: DetPid, thread: DetTid) -> bool {
        if !self.sigalrm.handled.contains(&process)
            || self.sigalrm.blocked.get(&thread) != Some(&false)
            || !self.sigalrm.pending.remove(&process)
        {
            return false;
        }
        self.sigalrm.lost.remove(&process);
        true
    }

    /// Forgets `thread`, and its process's ledger state when the thread was
    /// its process's leader: Linux drops pending signals at exit.
    pub(crate) fn retire_sigalrm_thread(&mut self, thread: DetTid, process: DetPid) {
        self.sigalrm.blocked.remove(&thread);
        self.sigalrm.polling.remove(&thread);
        if thread.as_raw() == process.as_raw() {
            self.sigalrm.handled.remove(&process);
            self.sigalrm.pending.remove(&process);
            self.sigalrm.lost.remove(&process);
        }
    }

    /// Wakes, reports or leaves `process`'s pending entry according to where its
    /// thread is.
    fn act_on_pending_sigalrm(&mut self, process: DetPid) {
        let thread = Self::sigalrm_owner(process);
        if !self.next_turns.contains_key(&thread) {
            // The process is gone; its pending signal goes with it.
            self.sigalrm.pending.remove(&process);
            self.sigalrm.lost.remove(&process);
            return;
        }
        match self.sigalrm.blocked.get(&thread) {
            // Blocked: the entry stays pending, as in Linux.
            Some(true) => return,
            Some(false) => {}
            // Every request of a process that handles SIGALRM carries the bit.
            // Without it, eligibility is unknown, so fail closed.
            None => {
                self.record_sigalrm_loss(process, thread, "its SIGALRM eligibility is unknown");
                return;
            }
        }
        if !matches!(self.thread_status(thread), ThreadStatus::NotRunning) {
            if self.in_polling_wait(thread) {
                self.record_sigalrm_loss(
                    process,
                    thread,
                    "a due SIGALRM found it in a polling wait that phase 1 cannot interrupt",
                );
            }
            // Otherwise running, or a filed request that has not blocked: a
            // pause is resolved at its admission, any other call takes the
            // entry at its completion.
            return;
        }
        if self.filed_pause(thread) {
            info!("[dtid {}] a pending SIGALRM wakes its pause.", thread);
            self.force_unblock_thread(thread, Self::sigalrm_wake(thread));
        } else {
            self.record_sigalrm_loss(
                process,
                thread,
                "a due SIGALRM found it blocked in a wait that phase 1 cannot interrupt",
            );
        }
    }

    /// The thread that owns `process`'s entry. In-guest processes are
    /// single-threaded, so it is the process's leader.
    fn sigalrm_owner(process: DetPid) -> DetTid {
        DetTid::from_raw(process.as_raw())
    }

    /// Whether `thread`'s process holds an entry that is due for it.
    fn sigalrm_due(&self, thread: DetTid) -> bool {
        let process = DetPid::from_raw(thread.as_raw());
        self.sigalrm.handled.contains(&process)
            && self.sigalrm.pending.contains(&process)
            && self.sigalrm.blocked.get(&thread) == Some(&false)
    }

    /// The request a SIGALRM wake substitutes for the thread's filed one; its
    /// grant answers `Signaled(SIGALRM)` (`unblock_guest`).
    fn sigalrm_wake(thread: DetTid) -> Resources {
        let mut wake = Resources::new(thread);
        wake.insert(
            ResourceID::InboundSignal(SigWrapper::from(Signal::SIGALRM)),
            Permission::W,
        );
        wake
    }

    /// Whether a determinism loss is recorded for `process`'s pending entry.
    #[cfg(test)]
    pub(crate) fn sigalrm_loss_recorded(&self, process: DetPid) -> bool {
        self.sigalrm.lost.contains(&process)
    }

    fn record_sigalrm_loss(&mut self, process: DetPid, thread: DetTid, why: &str) {
        if self.sigalrm.lost.insert(process) {
            crate::detlog::write_loss_notice(&format!(
                "thread {thread} of process {process} has a pending guest-handled SIGALRM, but {why}"
            ));
        }
    }

    /// Whether `thread`'s filed request is an emulated `pause`: a wait with no
    /// deadline (`SleepUntil(INDEFINITE)`) tagged `PAUSE_FYI` by `handle_pause`.
    /// Under in-guest LiteInst it is an ordinary request, since the backend
    /// offers no parked signal sites. A `nanosleep` whose deadline saturates
    /// files the same resource without the tag; it is not a pause.
    fn filed_pause(&self, thread: DetTid) -> bool {
        self.next_turns
            .get(&thread)
            .and_then(|turn| turn.req.try_read())
            .is_some_and(|request| {
                request.is_ok_and(|resources| {
                    resources.fyi == PAUSE_FYI
                        && resources.resources.keys().any(|resource| {
                        matches!(resource, ResourceID::SleepUntil(deadline) if deadline.is_indefinite())
                    })
                })
            })
    }
}
