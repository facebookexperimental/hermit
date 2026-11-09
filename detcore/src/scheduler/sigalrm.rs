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

/// A due SIGALRM entry reserved for one deterministic probe of a
/// polling pipe read (dh-ru056 slice 1, design revision 2 §5.1):
/// the entry stays in `pending`; this binds the probe target until
/// the probe's successor request is classified at the next step-2
/// sweep. In slice 1's single-thread scope the owner take is the
/// only consumer, so a reservation release path is deliberately
/// unreachable: the owned scheduler code asserts that rather than
/// inventing restore semantics for the pre-reservation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReservedPoll {
    /// Process whose pending entry is reserved.
    pub(crate) owner: DetPid,
    /// Only thread that may consume the reservation, by running
    /// the bound probe request.
    pub(crate) target: DetTid,
    /// Descriptor-table identity and fd of the bound read request.
    pub(crate) read_fd: (detcore_model::fd::FilesId, i32),
    /// Attempt lineage of the probe request as stored for its
    /// grant: (`poll_attempt == 0`, this `upgraded_from_attempt`).
    pub(crate) bound_upgraded_from_attempt: u32,
}

/// How the reserved probe's syscall pass ended, read from the
/// profile fields of the successor request it filed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReservedProbeOutcome {
    /// The successor is the next retry of the same read and the
    /// probe moved no bytes: substitute the wake (section 6 R2).
    NoProgressRetry,
    /// The successor is the next retry but the probe moved bytes:
    /// keep the reservation (section 5.1).
    ProgressRetry,
    /// The successor is not a retry of the bound read: the probe
    /// completed it (R1 post-result) — or a backend protocol
    /// violation, which also consumes here and records loss.
    Completed,
}

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
    /// The one reserved poll probe, if any (dh-ru056 slice 1).
    reserved_poll: Option<ReservedPoll>,
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

    /// The wake a no-progress reserved probe substitutes: the
    /// ordinary signal wake, suffixed in the durable marker trail
    /// so post-hoc audits can see that it came from the
    /// reserved-probe completion check.
    fn sigalrm_wake_probe(thread: DetTid) -> Resources {
        let mut wake = Self::sigalrm_wake(thread);
        wake.fyi("InboundSignal,SIGALRM (sigalrm wake).sigalrm_wake_probe");
        wake
    }

    /// Records `process`'s SIGALRM disposition, published by its runtime inside
    /// the turn of the call that changed it. A handler that becomes ignored or
    /// default discards a pending entry, as Linux discards a pending signal that
    /// becomes ignored. The runtime refuses the change to default while an
    /// entry is pending, so a default-acted signal is never discarded here.
    pub(crate) fn set_sigalrm_handled(&mut self, process: DetPid, handled: bool) {
        if !handled {
            self.assert_no_reserved_poll_release(
                process.as_raw(),
                "its disposition left the handler",
            );
        }
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
        if blocked {
            self.assert_no_reserved_poll_release(thread.as_raw(), "it blocked SIGALRM");
        }
        self.sigalrm.blocked.insert(thread, blocked);
    }

    /// Review gate G1: in slice 1 a reservation is never released back to
    /// `Pending`. Its target is single-threaded and holds no turn between
    /// reservation and probe (the probe is a retry inside the same read,
    /// not a syscall entry, so no mask or disposition publication of its
    /// own can arrive), and the reservation is classified at the next
    /// step 2 or at the successor's step-4 head, before that request is
    /// granted and so before any later publication or take. A
    /// publication or take that would release it therefore cannot occur;
    /// one that does is a defect of that argument, which stops the run
    /// rather than invent restore semantics for the pre-reservation
    /// request (design rev.2 section 5, approval gate G1).
    fn assert_no_reserved_poll_release(&self, raw: i32, what: &str) {
        if let Some(reserved) = self.sigalrm.reserved_poll {
            assert!(
                reserved.target.as_raw() != raw && reserved.owner.as_raw() != raw,
                "dh-ru056 slice 1 G1: the reserved pipe-read probe's thread {} would \
                 release its reservation because {what}; slice 1 keeps release unreachable",
                reserved.target
            );
        }
    }

    /// The target of the outstanding reserved poll probe, if any.
    pub(crate) fn sigalrm_reserved_poll_target(&self) -> Option<DetTid> {
        self.sigalrm.reserved_poll.map(|reserved| reserved.target)
    }

    /// Record the reservation created by the step-2 reserved-probe
    /// transition. Asserts the slice-1 single-flight discipline:
    /// a reservation already being outstanding would mean a second
    /// reserve raced the first probe, which placed precedence
    /// forbids by construction.
    pub(crate) fn record_sigalrm_reserved_poll(&mut self, reserved: ReservedPoll) {
        assert!(
            self.sigalrm.reserved_poll.is_none(),
            "dh-ru056 slice 1: a second reserved poll while one is outstanding"
        );
        self.sigalrm.reserved_poll = Some(reserved);
    }

    /// Take the outstanding reservation: the probe has run and its
    /// successor request is being classified. This consumes the
    /// reservation (not the ledger entry, which stays pending
    /// until its ordinary owner-turn take).
    pub(crate) fn take_sigalrm_reserved_poll(&mut self) -> Option<ReservedPoll> {
        self.sigalrm.reserved_poll.take()
    }

    /// Reserved-probe eligibility for the slice-1 pipe-read shape:
    /// exactly the request `detcore`'s pipe read files for its
    /// polling probes (`execute_nonblockable_fd_syscall`): an
    /// `InternalIOPolling` resource plus `read_fd`, restartable
    /// (`signal_interrupt_errno` is `ERESTARTSYS`, design rev.2
    /// section 5.1), with no earlier progress. Any other errno
    /// (a deadline wait's EINTR, or none) must never be replaced
    /// by the restart table's outcome; the phase-1 due-entry loss
    /// is preserved for it. Returns the request identity to bind,
    /// or `None` to keep the loss.
    fn reservable_pipe_read(request: &Resources) -> Option<(detcore_model::fd::FilesId, i32)> {
        if request.previous_attempt_made_progress
            || request.signal_interrupt_errno()
                != Some(reverie::syscalls::Errno::ERESTARTSYS.into_raw())
            || !request
                .resources
                .contains_key(&ResourceID::InternalIOPolling)
        {
            return None;
        }
        request.read_fd
    }

    /// Reserve `process`'s due entry for one deterministic probe
    /// of its polling pipe read (dh-ru056 slice 1 §5.1). The filed
    /// substate is normalized by the same upgrade step 4 applies
    /// (`upgraded_to_runnable`); the deferred-poller substate
    /// keeps its stored request untouched and is only restored to
    /// the run queue. Nothing is substituted here: the probe must
    /// run first so a writer commit that preceded the expiry is
    /// observed by the read (R1) instead of being masked by an
    /// early wake. Returns whether a reservation was created.
    fn reserve_sigalrm_poll(&mut self, process: DetPid, thread: DetTid) -> bool {
        // Single flight: another process's probe is outstanding (its
        // owner's sweep returns before reaching here). Slice 1 holds
        // one reservation, so this entry keeps phase 1's loss rather
        // than compete with that probe for placed precedence.
        if self.sigalrm.reserved_poll.is_some() {
            return false;
        }
        let deferred = self.sigalrm.polling.contains(&thread);
        let filed = self
            .next_turns
            .get(&thread)
            .and_then(|turn| turn.req.try_read())
            .filter(|request| request.is_ok())
            .map(|request| request.unwrap());
        let Some(mut request) = filed else {
            return false;
        };
        let Some(read_fd) = Self::reservable_pipe_read(&request) else {
            return false;
        };
        // The third slice-1 admission gate: alarm-taker placement
        // must actually put the probe ahead of ordinary runnable
        // work. If step 3 would consume this request as an inbound
        // signal turn, or the alarm priority is unavailable, a
        // writer could run between reservation and probe — keep
        // phase 1's loss instead of claiming placed precedence.
        if !self.alarm_priority_available() || !self.takes_alarm_priority(thread) {
            return false;
        }
        let bound_upgraded_from_attempt = request.upgraded_from_attempt.max(request.poll_attempt);
        self.record_sigalrm_reserved_poll(ReservedPoll {
            owner: process,
            target: thread,
            read_fd,
            bound_upgraded_from_attempt,
        });
        // Filed substate: normalize with the same upgrade step 4
        // applies, in place, so the stored request becomes the
        // runnable probe (attempt 0). In place, not
        // `substitute_request`: substituting would mint a second
        // Ivar in between that retired/Cancelled the live
        // request's response token. Alarm-taker registration is
        // the placement (primary): the probe is already queued in
        // both substates, so no run-queue re-insertion is needed.
        if !deferred {
            request = request.upgraded_to_runnable();
            let turn = self
                .next_turns
                .get_mut(&thread)
                .expect("reservation target has a filed request");
            turn.req = crate::ivar::Ivar::full(Ok(request));
        }
        self.parked.alarm_takers.insert(process, thread);
        true
    }

    /// Successor classification of an outstanding reserved probe,
    /// from the scheduler-owned profile fields of the request the
    /// probe's syscall pass filed. `None` means no classifiable
    /// successor has arrived yet: the stored request still reads
    /// as the bound probe through every observable field (an
    /// actual retry whose syscall pass already ran differs by
    /// `poll_attempt`/`upgraded_from_attempt` lineage; a
    /// completion differs by resource/read_fd shape).
    fn peek_sigalrm_reserved_poll_result(&self) -> Option<(ReservedPoll, ReservedProbeOutcome)> {
        let reserved = self.sigalrm.reserved_poll?;
        let successor = self
            .next_turns
            .get(&reserved.target)
            .and_then(|turn| turn.req.try_read())
            .filter(|request| request.is_ok())
            .map(|request| request.unwrap())?;
        let is_retry = successor
            .resources
            .contains_key(&ResourceID::InternalIOPolling)
            && successor.read_fd == Some(reserved.read_fd);
        if !is_retry {
            return Some((reserved, ReservedProbeOutcome::Completed));
        }
        if successor.poll_attempt == 0
            && successor.upgraded_from_attempt == reserved.bound_upgraded_from_attempt
        {
            // The stored request is still the bound probe itself:
            // the probe has not granted, so no successor exists
            // yet to classify.
            return None;
        }
        Some((
            reserved,
            if successor.previous_attempt_made_progress {
                ReservedProbeOutcome::ProgressRetry
            } else {
                ReservedProbeOutcome::NoProgressRetry
            },
        ))
    }

    /// Act on a classified reserved probe at the step-2 completion
    /// check: substitute the successor retry with the signal wake
    /// (section 6 R2 wake/state), suffixed
    /// `.sigalrm_wake_probe` in the durable run-marker trail so a
    /// substitution before a data-ready probe remains visible.
    fn act_on_sigalrm_reserved_poll(
        &mut self,
        (reserved, outcome): (ReservedPoll, ReservedProbeOutcome),
    ) {
        match outcome {
            ReservedProbeOutcome::ProgressRetry => {
                // G1: a single blocking pipe read that moved bytes
                // returns; it cannot still be polling the same read.
                // Seeing progress here means a second attached probe
                // ran between reservation and classification, which
                // placed precedence forbids in slice 1 — a protocol
                // violation, not a state to carry across retries.
                self.record_sigalrm_reserved_poll_loss(
                    reserved.owner,
                    reserved.target,
                    "its reserved pipe-read probe reported progress but re-polled, \
                     which slice 1 keeps unrepresentable (G1)",
                );
            }
            ReservedProbeOutcome::NoProgressRetry => {
                let taken = self.take_sigalrm_reserved_poll();
                debug_assert_eq!(taken, Some(reserved));
                // The successor retry is still queued (filed by
                // the probe's syscall pass or the harness);
                // dequeue it so force-unblock's requeue is its
                // only queue entry.
                self.run_queue.remove_tid(reserved.target);
                self.force_unblock_thread(
                    reserved.target,
                    Self::sigalrm_wake_probe(reserved.target),
                );
            }
            ReservedProbeOutcome::Completed => {
                let taken = self.take_sigalrm_reserved_poll();
                debug_assert_eq!(taken, Some(reserved));
                // The probe completed: its result stands (R1) and
                // the entry remains pending for the ordinary
                // post-result completion delivery — except when
                // the thread/process vanished around the probe,
                // which act_on_pending would have retired instead.
            }
        }
    }

    /// Record determinism loss for `process`'s pending entry (the
    /// reserved-probe completion check's protocol-violation arm).
    pub(crate) fn record_sigalrm_reserved_poll_loss(
        &mut self,
        process: DetPid,
        thread: DetTid,
        why: &str,
    ) {
        // A consumed reservation whose probe turn then cannot be
        // accounted for: settle it before recording the loss.
        if self.sigalrm.reserved_poll.map(|r| r.target) == Some(thread) {
            let _ = self.take_sigalrm_reserved_poll();
        }
        self.record_sigalrm_loss(process, thread, why);
    }

    /// Settles an outstanding reservation of `thread` when its
    /// successor request reaches step 4 as the selected head before a
    /// step-2 completion check classified it — the usual case, since
    /// step 3 reselects the probe's thread and awaits that request
    /// (G2: still before step 4 can defer it). Returns whether the
    /// wake was substituted: step 4 must then end this turn rather
    /// than act on `thread`'s stale retry, so the selection step 3
    /// opened is committed first, and the substitution's requeue is
    /// the thread's only queue entry. A completed or progressed probe
    /// leaves the selection open and step 4 goes on with the head
    /// request.
    pub(super) fn act_on_sigalrm_reserved_head(&mut self, thread: DetTid) -> bool {
        let Some(element) = self
            .peek_sigalrm_reserved_poll_result()
            .filter(|(reserved, _)| reserved.target == thread)
        else {
            return false;
        };
        let substitutes = element.1 == ReservedProbeOutcome::NoProgressRetry;
        if substitutes {
            let popped = self.run_queue.commit_tentative_pop();
            assert_eq!(popped, thread, "step 4's head is the selected thread");
        }
        self.act_on_sigalrm_reserved_poll(element);
        substitutes
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
        // Reserved-probe completion check (dh-ru056 slice 1 §5.1,
        // review gate G2): classify the probe's successor request
        // before the sweep acts on any other entry, and so before
        // step 4 can defer the successor. A settled reservation is
        // then free for another process the sweep finds due in a
        // polling read (single flight). A reservation the sweep
        // creates below has no successor yet, and a silent
        // outstanding one means the probe has not granted: either
        // stays outstanding until a later pass.
        if let Some(reserved) = self.peek_sigalrm_reserved_poll_result() {
            self.act_on_sigalrm_reserved_poll(reserved);
        }
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
        self.assert_no_reserved_poll_release(process.as_raw(), "its entry was taken unclassified");
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
        // A reservation bound to the retiring thread dies with it:
        // its probe will never grant, and a stale reservation
        // would block every later reserve (failed wake, G2).
        if self.sigalrm.reserved_poll.map(|r| r.target) == Some(thread) {
            let _ = self.take_sigalrm_reserved_poll();
        }
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
            if self.sigalrm_reserved_poll_target() == Some(thread) {
                // Outstanding reservation: the probe has not yet
                // produced a successor; leave pending, no loss.
                return;
            }
            if self.in_polling_wait(thread) {
                // dh-ru056 slice 1 (§5.1): reserve a deterministic
                // probe for eligible pipe reads instead of
                // recording the loss the phase-1 sweep recorded.
                if !self.reserve_sigalrm_poll(process, thread) {
                    self.record_sigalrm_loss(
                        process,
                        thread,
                        "a due SIGALRM found it in a polling wait that phase 1 cannot interrupt",
                    );
                }
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
