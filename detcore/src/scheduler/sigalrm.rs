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
//!   A running thread, or one whose filed request has not blocked, takes the
//!   entry at its syscall's completion; that delivery is the runtime's.
//! - One polling wait is modelled instead (dev-hermit dh-ru056 slice 1,
//!   signal phase-2 design revision 2, section 5.1): a restartable pipe read
//!   retried after EAGAIN (`InternalIOPolling` with a `read_fd`) on a
//!   descriptor only guest actions make ready, outside record and replay. The
//!   entry is reserved to the owner (`ReservedPoll`) and the filed retry made
//!   runnable. While that probe is unrun, step 3 selects its thread right
//!   after a group-exit decider (`reserved_probe_to_resume`), so no other
//!   thread's turn commits between the reservation and the probe. The probe's
//!   successor request is classified at the next pass's step 2, before the
//!   sweep (step 1 waits for every queued thread to file), or defensively at
//!   that request's step-4 head:
//!   - a retry that moved nothing is replaced by the `InboundSignal` wake, at
//!     the front of its band (only: a better band or a chaos draw can still
//!     run first, and the wake grant's own attempt then decides, R1);
//!   - a retry that moved bytes is a protocol violation charged as a loss
//!     (G1);
//!   - a new read of the same descriptor is charged as a loss;
//!   - any other successor that is not a retry is classified completed
//!     without further checks: in production the delivery at the read's
//!     completion, or the request of a scheduler wake that replaced the
//!     unrun probe (requeued in its band, possibly at the back, not as a
//!     placed taker). The entry stays pending for delivery at a completion.
//!
//!   An unrun probe whose thread is out of the run queue when step 3 begins
//!   is released with a loss (`release_unqueued_sigalrm_reservation`; no
//!   production path is known to reach it). Every other polling wait, a read
//!   that does not pass these checks, an entry whose process already has a
//!   loss recorded (kept per process: one the runtime reports after a take
//!   also covers that process's later entries), and a second process's entry
//!   while one probe is reserved, keep the loss.
//! - Every change happens at a scheduler commit: an expiry, an admission, the
//!   step-2 completion check and sweep, the step-3 selection, or the
//!   successor's step-4 head. So what is pending, what is due, and what is
//!   reserved is a function of the schedule. One exception, shared with the
//!   ledger's existing retirement: a thread's retirement
//!   (`retire_sigalrm_thread`) runs from `logically_kill_thread`, outside the
//!   turns.
//!
//! Nothing publishes a handled SIGALRM until the runtime admits guest
//! handlers. Phase 1 admits a pipe read in a handling process only on an end
//! only guests hold, while no descriptor may have left the guest, with the
//! reader's copy of the description's status the latest any process set,
//! outside record and replay and schedule replay, and under a handler
//! without `SA_RESTART`
//! (`sigalrm_phase1::descriptor_class`, dh-ru056 slice 2); no other read
//! reaches the reservation. An admitted read whose guest-held marks an export
//! revokes while it waits stops the run by name when its entry falls due
//! (`SigalrmLedger::stop`), and so does one whose probe's wake is still
//! filed, ungranted, when the export commits (`SigalrmLedger::marked_wake`).
//! An entry that finds another process's probe reserved waits, with no loss,
//! for a later sweep.

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
use crate::types::OpenFileId;
use crate::types::SigWrapper;

/// A due SIGALRM entry reserved for one deterministic probe of a
/// polling pipe read (dh-ru056 slice 1, design revision 2 §5.1):
/// the entry stays in `pending`; this binds the probe target until
/// the probe's successor request is classified (at the next pass's
/// step 2, before the sweep, or at that request's step-4 head), the
/// target retires, or an unqueued unrun probe is released with a
/// loss at step 3. Nothing else ends it: a take or a mask
/// publication that would release it unclassified is deliberately
/// unreachable, and the owned scheduler code asserts that (G1)
/// rather than invent restore semantics for the pre-reservation
/// request.
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
    /// The successor is the next retry but the probe moved bytes: a
    /// single blocking pipe read that moved bytes returns, so this is
    /// a protocol violation slice 1 charges as a loss (G1).
    ProgressRetry,
    /// Any successor that is not a retry of the bound read, not checked
    /// further: the probe's read ended without the reserved wake. In
    /// production it is the due entry's delivery filed by the read's
    /// completion (R1, post-result), or the request of a scheduler wake
    /// that replaced the unrun probe, requeued in its band (a cross-task
    /// drain wake goes to the back of it) rather than as a placed taker.
    /// Records no loss; the entry stays pending for delivery at a
    /// completion.
    Completed,
    /// The successor is a first attempt of another lineage on the same
    /// descriptor: a new read began after a completion that delivered
    /// no due entry, which the completion path never does with an entry
    /// due (section 5.1 protocol violation). Records a loss.
    NewRead,
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
    /// The status flags last published for each pipe read description
    /// (`SigalrmControl::PublishPipeStatus`): the description's status, which
    /// a process's own copy may not show after a fork (dh-ru056 slice 2,
    /// review P1-b), with the processes that may hold it. An entry lasts
    /// while any of them does (`PipeStatus::holders`).
    pipe_status: BTreeMap<OpenFileId, PipeStatus>,
    /// The thread whose filed request is the signal wake a no-progress probe
    /// substituted (`act_on_sigalrm_reserved_poll`) for a polled pipe read
    /// that relied on guest-held marks, until that wake is granted or
    /// replaced (`end_sigalrm_polling_wait`). The wake request names neither
    /// the read nor its descriptor, so this is what tells the sweep that the
    /// wake's final attempt still rests on those marks (review of 29b3e6a8,
    /// finding 2).
    marked_wake: Option<DetTid>,
    /// The named refusal that ends the run before the next selection, set
    /// when a due entry finds an admitted pipe read whose guest-held
    /// authority was revoked while it waited (`act_on_pending_sigalrm`,
    /// review P1-a). Taken by the turn loop (`take_sigalrm_stop`).
    stop: Option<String>,
}

/// The latest status of one pipe read description (`SigalrmLedger::pipe_status`).
#[derive(Debug, Default)]
struct PipeStatus {
    /// Its status flags, as the last process to set them published.
    flags: i32,
    /// Whether its status can no longer be followed: it may have left a
    /// process over `SCM_RIGHTS` (`SigalrmControl::UntrackPipeStatus`), or a
    /// change was published without the serial grant. Every copy then
    /// differs (`SigalrmControl::PipeStatusDiffers`).
    untracked: bool,
    /// The processes that may hold the description: its creator, which
    /// publishes it at the `pipe2` that made it, each process a holder's fork
    /// creates (`Scheduler::sigalrm_process_created`), and any later
    /// publisher. A holder leaves at the close of its last descriptor of it
    /// (`Scheduler::release_pipe_status`, from the release Detcore sends for
    /// every description's last local reference) or at its retirement
    /// (`Scheduler::retire_sigalrm_thread`), and the entry goes with the last
    /// one, so the ledger keeps only descriptions some process may still read
    /// while a copy another process changed stays visible to every alias
    /// (review of 29b3e6a8, finding 3). A process that receives the
    /// description over `SCM_RIGHTS` is not counted until it publishes, which
    /// is sound because that send revoked guest-held marks first, and phase 1
    /// admits no read once they are revoked.
    holders: BTreeSet<DetPid>,
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
            SigalrmControl::PublishPipeStatus { id, flags } => {
                // A change published without the serial grant, while a
                // process handles SIGALRM, was recorded at a moment the host
                // chose: fail closed rather than keep it as the latest.
                let untimed = !self.sigalrm.handled.is_empty() && !self.holds_serial_grant(thread);
                let status = self.sigalrm.pipe_status.entry(id).or_default();
                status.flags = flags;
                status.untracked |= untimed;
                status.holders.insert(process);
                return false;
            }
            SigalrmControl::UntrackPipeStatus { id } => {
                let status = self.sigalrm.pipe_status.entry(id).or_default();
                status.untracked = true;
                status.holders.insert(process);
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
            SigalrmControl::GuestHeldMarksRevoked => return self.guest_held_marks_revoked,
            SigalrmControl::PipeStatusDiffers { id, flags } => {
                return self
                    .sigalrm
                    .pipe_status
                    .get(&id)
                    .is_some_and(|published| published.untracked || published.flags != flags);
            }
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
            | SigalrmControl::EntryDeliveryUnfaithful
            | SigalrmControl::GuestHeldMarksRevoked
            | SigalrmControl::PublishPipeStatus { .. }
            | SigalrmControl::UntrackPipeStatus { .. }
            | SigalrmControl::PipeStatusDiffers { .. } => unreachable!("answered above"),
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
    /// `Pending`. The argument rests on this ordering, which the code
    /// enforces where it says so. The target is single-threaded. From the
    /// reservation (step 2) until its probe is granted, step 3 selects no
    /// other thread except a group-exit decider (`reserved_probe_to_resume`),
    /// and the target itself holds no turn; the probe is a retry inside the
    /// same read, not a syscall entry, so no mask or disposition publication
    /// of its own runs in it. If the target is out of the run queue when step
    /// 3 begins, the reservation is released with a loss, not back to
    /// `Pending` (`release_unqueued_sigalrm_reservation`). Any successor that
    /// is not a retry, including the request of a scheduler wake that
    /// replaced the unrun probe (requeued in its band, possibly at the back),
    /// is classified completed, without further checks, before it is
    /// granted, and the entry stays pending. The successor
    /// request is filed before the next pass's step 2 (step 1 waits for every
    /// queued thread to file) and classified there, before the sweep, or at
    /// that request's step-4 head; either way before the request is granted.
    /// A completed read's successor is its delivery's `InboundSignal` turn
    /// (`Detcore::complete_syscall`, `deliver_due_sigalrm`), so the take in
    /// that turn comes after the classification. A publication or take that
    /// would release the reservation therefore cannot occur; one that does
    /// is a defect of this argument, which stops the run rather than invent
    /// restore semantics for the pre-reservation request (design rev.2
    /// section 5, approval gate G1).
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
    /// section 5.1), with no earlier progress, on a descriptor whose
    /// readiness only guest actions change (`polls_only_guest_state`,
    /// and not a named FIFO a process outside the container may hold):
    /// otherwise whether the probe finds data or the wake substitutes
    /// would depend on host timing. Any other errno (a deadline wait's
    /// EINTR, or none) must never be replaced by the restart table's
    /// outcome. Not under record or replay: there a polled read keeps the
    /// guest-state authority taken at its entry, which descriptor exports do
    /// not revoke (`execute_nonblockable_fd_syscall`), and the record/replay
    /// test the design requires (D6) does not exist yet. Returns the request
    /// identity to bind, or `None` to keep phase 1's loss.
    fn reservable_pipe_read(
        &self,
        request: &Resources,
    ) -> Option<(detcore_model::fd::FilesId, i32)> {
        if request.previous_attempt_made_progress
            || request.signal_interrupt_errno()
                != Some(reverie::syscalls::Errno::ERESTARTSYS.into_raw())
            || !request
                .resources
                .contains_key(&ResourceID::InternalIOPolling)
            || !self.polls_only_guest_state(request)
            || request.polls_a_named_fifo
            || self.recordreplay_modes
            || self.replayer.is_some()
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
        // Single flight: another process's probe is outstanding. The
        // sweep waits for it before calling here (`act_on_pending_sigalrm`,
        // no loss); this check keeps the single flight if a caller does
        // not.
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
        let Some(read_fd) = self.reservable_pipe_read(&request) else {
            return false;
        };
        // The third slice-1 admission gate. Placed precedence is enforced
        // where step 3 selects (`reserved_probe_to_resume`): the probe
        // resumes right after a group-exit decider while it is unrun. So
        // the reservation needs only what that selection and the poller
        // restore below need: no open selection (the restore is a queue
        // operation) and the thread queued to run. An entry whose loss is
        // already recorded is not reserved again: the loss stands, and the
        // entry is delivered at a completion as before. `lost` is kept per
        // process, not per entry: a loss the runtime reports after its take
        // (`DeliveryLost`, `EntryDeliveryUnfaithful`) stays recorded until
        // the next take, so a later, distinct entry of that process is
        // refused here too, with no second notice. Fail-closed and
        // deterministic; phase 1's own rule, which this slice leaves as is.
        if self.run_queue.tentative_pop_in_progress()
            || !self.run_queue.contains_tid(thread)
            || self.sigalrm.lost.contains(&process)
        {
            return false;
        }
        let bound_upgraded_from_attempt = request.upgraded_from_attempt.max(request.poll_attempt);
        self.record_sigalrm_reserved_poll(ReservedPoll {
            owner: process,
            target: thread,
            read_fd,
            bound_upgraded_from_attempt,
        });
        if deferred {
            // Deferred poller: step 4 already rewrote the stored request
            // to its runnable copy and backed the thread off. Restore its
            // normal priority and drop its poll deadline (section 5.1);
            // the stored request is not rewritten.
            self.run_queue.restore_poller_priority(thread);
            self.forget_poll_deadline(thread);
        } else {
            // Filed substate: normalize with the same upgrade step 4
            // applies, in place, so the stored request becomes the
            // runnable probe (attempt 0). In place, not
            // `substitute_request`: substituting would mint a second
            // Ivar in between that retired/Cancelled the live
            // request's response token.
            request = request.upgraded_to_runnable();
            let turn = self
                .next_turns
                .get_mut(&thread)
                .expect("reservation target has a filed request");
            turn.req = crate::ivar::Ivar::full(Ok(request));
        }
        debug_assert_eq!(
            self.reserved_probe_to_resume(),
            Some((process, thread)),
            "a new reservation's probe is unrun and queued for step 3"
        );
        true
    }

    /// The reserved probe step 3 resumes next, with its process: the
    /// outstanding reservation's target while its stored request is still
    /// the unrun bound probe (`InternalIOPolling` on the bound `read_fd`,
    /// attempt 0 at the bound lineage) and it is queued. Step 3 selects it
    /// right after a group-exit decider (design rev.2 section 5, placed
    /// precedence: the `sigchld_takers` class), ahead of signal takers,
    /// alarm takers, `rt_sigreturn` continuations and a vfork child, so no
    /// other thread's turn commits between the reservation and the probe.
    /// Once the probe is granted, or a wake replaced it, this is `None`.
    pub(super) fn reserved_probe_to_resume(&self) -> Option<(DetPid, DetTid)> {
        let reserved = self.unrun_sigalrm_reserved_probe()?;
        self.run_queue
            .contains_tid(reserved.target)
            .then_some((reserved.owner, reserved.target))
    }

    /// The outstanding reservation while its target's stored request is
    /// still the unrun bound probe, queued or not.
    fn unrun_sigalrm_reserved_probe(&self) -> Option<ReservedPoll> {
        let reserved = self.sigalrm.reserved_poll?;
        let request = self
            .next_turns
            .get(&reserved.target)
            .and_then(|turn| turn.req.try_read())
            .and_then(Result::ok)?;
        let unrun = request
            .resources
            .contains_key(&ResourceID::InternalIOPolling)
            && request.read_fd == Some(reserved.read_fd)
            && request.poll_attempt == 0
            && request.upgraded_from_attempt == reserved.bound_upgraded_from_attempt;
        unrun.then_some(reserved)
    }

    /// Fails closed on an unrun reserved probe whose thread is out of the
    /// run queue when step 3 begins: step 3 cannot select it, so its
    /// precedence would be outlived by every turn committed meanwhile.
    /// Releases the reservation through the loss path
    /// (`record_sigalrm_reserved_poll_loss`), never back to `Pending`
    /// (G1). No production path is known to reach this: the reservation
    /// needs its thread queued, step 3 selects it next, and its only
    /// happens-before hold is its own checkpoint, a successor request the
    /// step-4 head classifies before the hold parks it.
    pub(super) fn release_unqueued_sigalrm_reservation(&mut self) {
        let Some(reserved) = self.unrun_sigalrm_reserved_probe() else {
            return;
        };
        if self.run_queue.contains_tid(reserved.target) {
            return;
        }
        self.record_sigalrm_reserved_poll_loss(
            reserved.owner,
            reserved.target,
            "its reserved pipe-read probe left the run queue before it ran",
        );
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
        if successor.poll_attempt == 0 {
            if successor.upgraded_from_attempt == reserved.bound_upgraded_from_attempt {
                // The stored request is still the bound probe itself:
                // the probe has not granted, so no successor exists
                // yet to classify.
                return None;
            }
            // A first attempt of another lineage: a new read of the
            // same descriptor began, so the bound read ended without
            // its due entry's delivery. A retry of the bound read
            // advances its lineage.
            return Some((reserved, ReservedProbeOutcome::NewRead));
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
                // Seeing progress here is a protocol violation, not a
                // state to carry across retries. (Defensive: slice 1
                // reserves only a read with no earlier progress, and
                // the probe is its one attempt.)
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
                // Whether the read's readiness rested on guest-held marks,
                // read from the retry before the wake replaces it.
                let relies_on_marks = self
                    .next_turns
                    .get(&reserved.target)
                    .and_then(|turn| turn.req.try_read())
                    .is_some_and(|request| {
                        request.is_ok_and(|request| request.relies_on_guest_held_marks)
                    });
                // The successor retry is still queued (filed by
                // the probe's syscall pass or the harness);
                // dequeue it so force-unblock's requeue is its
                // only queue entry.
                self.run_queue.remove_tid(reserved.target);
                // Front of its band only: a thread in a better band,
                // or a targeted-chaos draw (`force_unblock_thread_at`),
                // can still run before the wake, and the wake grant's
                // own attempt then decides (R1: data that arrived in
                // between is returned). Placed precedence for the wake
                // itself would break the bound of
                // https://github.com/rrnewton/hermit/issues/4929 (design
                // section 5.1 amendment pending with its owner).
                let requeued = self.force_unblock_thread_at(
                    reserved.target,
                    Self::sigalrm_wake_probe(reserved.target),
                    true,
                );
                debug_assert!(
                    requeued,
                    "the no-progress wake requeues the reserved probe's thread"
                );
                // The wake's grant makes the read's final attempt, which
                // still rests on those marks: an export committed before that
                // grant stops the run (`act_on_pending_sigalrm`).
                if requeued && relies_on_marks {
                    self.sigalrm.marked_wake = Some(reserved.target);
                }
            }
            ReservedProbeOutcome::Completed => {
                let taken = self.take_sigalrm_reserved_poll();
                debug_assert_eq!(taken, Some(reserved));
                // The read ended without a wake: its result stands
                // (R1) and the entry remains pending for the delivery
                // at its completion (the successor itself, or the
                // completion of the call a scheduler wake ended).
            }
            ReservedProbeOutcome::NewRead => {
                self.record_sigalrm_reserved_poll_loss(
                    reserved.owner,
                    reserved.target,
                    "its reserved pipe-read probe's read ended and a new read began \
                     without the due entry's delivery (section 5.1)",
                );
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

    /// Settles an outstanding reservation of `thread` when its successor
    /// request reaches step 4 as the selected head unclassified. In
    /// production the step-2 check classifies the successor first (step 1
    /// waits for every queued thread to file), so this is the defensive path
    /// for a successor filed after that check (G2: still before step 4 can
    /// defer it). Returns whether the wake was substituted: step 4 must then
    /// end this turn rather than act on `thread`'s stale retry, so the
    /// selection step 3 opened is committed first, and the substitution's
    /// requeue is the thread's only queue entry. A completed or progressed
    /// probe leaves the selection open and step 4 goes on with the head
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
        if self.sigalrm.marked_wake == Some(thread) {
            self.sigalrm.marked_wake = None;
        }
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
        if self.sigalrm.marked_wake == Some(thread) {
            self.sigalrm.marked_wake = None;
        }
        if thread.as_raw() == process.as_raw() {
            self.sigalrm.handled.remove(&process);
            self.sigalrm.pending.remove(&process);
            self.sigalrm.lost.remove(&process);
            // A retired process holds no descriptor.
            self.sigalrm.pipe_status.retain(|_, status| {
                status.holders.remove(&process);
                !status.holders.is_empty()
            });
        }
    }

    /// `child`, a new process, was created by `creator`'s thread: it holds a
    /// copy of every descriptor its creator's process holds, as Linux's fork
    /// copies the table (`dup_fd`), so it may hold every pipe description
    /// that process may hold (`PipeStatus::holders`).
    pub(crate) fn sigalrm_process_created(&mut self, creator: DetTid, child: DetPid) {
        let parent = self.sigchld_process(creator);
        for status in self.sigalrm.pipe_status.values_mut() {
            if status.holders.contains(&parent) {
                status.holders.insert(child);
            }
        }
    }

    /// `thread`'s process closed its last descriptor of open file description
    /// `id` (the release Detcore sends for a description's last local
    /// reference, `GlobalRequest::ReleasePort`): it no longer holds that
    /// description, and a pipe status no process holds is forgotten.
    pub(crate) fn release_pipe_status(&mut self, thread: DetTid, id: OpenFileId) {
        let process = self.sigchld_process(thread);
        if let Some(status) = self.sigalrm.pipe_status.get_mut(&id) {
            status.holders.remove(&process);
            if status.holders.is_empty() {
                self.sigalrm.pipe_status.remove(&id);
            }
        }
    }

    /// The processes that may hold pipe description `id`, while its status is
    /// recorded.
    #[cfg(test)]
    pub(crate) fn pipe_status_holders(&self, id: OpenFileId) -> Option<Vec<DetPid>> {
        self.sigalrm
            .pipe_status
            .get(&id)
            .map(|status| status.holders.iter().copied().collect())
    }

    /// How many pipe descriptions have a recorded status.
    #[cfg(test)]
    pub(crate) fn pipe_status_len(&self) -> usize {
        self.sigalrm.pipe_status.len()
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
        if self.guest_held_marks_revoked && self.sigalrm.marked_wake == Some(thread) {
            // A no-progress probe of a read admitted on guest-held marks was
            // replaced by the signal wake, whose grant makes the read's final
            // attempt; the wake is not placed, so a better band could run
            // first, and its export has revoked the marks that attempt rests
            // on. Whether it finds data or returns EINTR may now depend on a
            // process outside the guest: stop the run by name before the wake
            // is granted (review of 29b3e6a8, finding 2).
            self.stop_for_revoked_authority(thread);
            return;
        }
        if !matches!(self.thread_status(thread), ThreadStatus::NotRunning) {
            if self.sigalrm_reserved_poll_target() == Some(thread) {
                // Outstanding reservation: the probe has not yet
                // produced a successor; leave pending, no loss.
                return;
            }
            if self.in_polling_wait(thread) {
                if self.waits_on_revoked_guest_held_marks(thread) {
                    // Phase 1 admitted this pipe read on guest-held marks
                    // (its gate refuses one once they are revoked), and an
                    // export revoked them while it waited: whether its probe
                    // finds data may now depend on a process outside the
                    // guest, so no probe is reserved. Linux would interrupt
                    // the read here; leaving it to wait, with only a loss
                    // recorded, could hold it until data or forever. Stop the
                    // run by name instead (dh-ru056 slice 2, review P1-a).
                    self.stop_for_revoked_authority(thread);
                    return;
                }
                if self.sigalrm.reserved_poll.is_some() {
                    // Single flight: another process's probe is outstanding
                    // (this thread's own returned above). The entry stays
                    // pending with no loss, and every later sweep tries
                    // again (step 2 sweeps every pending entry each pass),
                    // so it is reserved once that probe settles, within that
                    // probe's own lifetime: its unrun probe resumes at the
                    // next selection with placed precedence, and the next
                    // pass's completion check settles it. Nothing is released
                    // back to pending, since nothing was reserved (G1); a
                    // read that ends meanwhile delivers the entry at its
                    // completion, as any call does (coord7 review of
                    // ec9b2177, S3).
                    return;
                }
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

    /// Whether `thread`'s filed request is a polled read whose guest-state
    /// readiness rested on guest-held marks that a descriptor export has
    /// since revoked (`Scheduler::polls_only_guest_state`).
    fn waits_on_revoked_guest_held_marks(&self, thread: DetTid) -> bool {
        self.guest_held_marks_revoked
            && self
                .next_turns
                .get(&thread)
                .and_then(|turn| turn.req.try_read())
                .is_some_and(|request| {
                    request.is_ok_and(|request| {
                        request.read_fd.is_some()
                            && request.polls_guest_state_only
                            && request.relies_on_guest_held_marks
                            && request
                                .resources
                                .contains_key(&ResourceID::InternalIOPolling)
                    })
                })
    }

    /// Sets the run's named stop for `thread`'s pipe read, whose guest-held
    /// authority was revoked, unless a stop is already set.
    fn stop_for_revoked_authority(&mut self, thread: DetTid) {
        if self.sigalrm.stop.is_none() {
            let refusal = crate::refused_signal_controls::sigalrm_pipe_authority_revoked_refusal(
                thread.as_raw(),
            );
            tracing::error!("[sched] {}", refusal);
            self.sigalrm.stop = Some(refusal);
        }
    }

    /// Whether a due entry set a named refusal for the run that the turn
    /// loop has not taken yet (`take_sigalrm_stop`).
    pub(crate) fn sigalrm_stop_pending(&self) -> bool {
        self.sigalrm.stop.is_some()
    }

    /// Takes the named refusal a due entry set for the run
    /// (`SigalrmLedger::stop`); the turn loop then ends the run with it.
    pub(crate) fn take_sigalrm_stop(&mut self) -> Option<String> {
        self.sigalrm.stop.take()
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
