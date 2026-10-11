/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A logical parked request can use several single-use RPC responses. Only the
//! scheduler daemon moves its queue entry; RPC handlers post bounded intents.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use reverie::CallbackSignalSite;
use reverie::ChildExitCompletion;
use reverie::ChildExitPublicationResult;
use reverie::ExitStatus;
use reverie::ParkedObservationLease;
use reverie::PreparedSignalToken;
use reverie::ProcessSignalPublicationResult;
use reverie::SignalDeliveryPermit;
use reverie::SignalEvent;
use reverie::SignalProcessId;
use reverie::SignalTarget;
use serde::Deserialize;
use serde::Serialize;

use super::RealTimeFate;
use super::SchedRequest;
use super::SchedResponse;
use super::Scheduler;
use super::ThreadNextTurn;
use super::kernel_signal_bit;
use super::real_timer::TimerFailure;
use super::runqueue::SuspendedRunQueueEntry;
use super::sleep_signal::SleeperStateUnreadable;
use super::timed_waiters;
use super::timed_waiters::SignalTimerId;
use crate::ivar::Ivar;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::tool_global::ResumeStatus;
use crate::types::ChildWaitExitClass;
use crate::types::ChildWaitSpec;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::MmId;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ParkedWaitPolicy {
    NanosleepNoHandlerRestart { absolute_deadline: LogicalTime },
    PauseNoHandlerRestart,
}
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Serialize,
    Deserialize
)]
pub struct ContinuationId {
    pub dettid: DetTid,
    pub nonce: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum RpcOrigin {
    DirectRequestResources,
    ResumeParkedRequest {
        continuation: ContinuationId,
        cycle: u64,
    },
    ParentContinue,
    TraceSchedEvent,
    FutexAction,
    ThreadStart,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ControlCapability {
    None,
    ParkedWait {
        policy: ParkedWaitPolicy,
        site: CallbackSignalSite,
    },
    /// A witnessed EAGAIN from the exact original scalar read, with no bytes transferred.
    PolledRead {
        site: CallbackSignalSite,
    },
    /// A serial terminal wait whose complete selector remains scheduler-owned.
    ChildWait {
        site: CallbackSignalSite,
    },
    PublishOnly {
        lease: ParkedObservationLease,
        site: CallbackSignalSite,
    },
}
impl ControlCapability {
    fn site(self) -> Option<CallbackSignalSite> {
        match self {
            Self::None => None,
            Self::ParkedWait { site, .. }
            | Self::PolledRead { site }
            | Self::ChildWait { site }
            | Self::PublishOnly { site, .. } => Some(site),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResourceOrigin {
    pub rpc: RpcOrigin,
    pub mm: MmId,
    pub control: ControlCapability,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum NextTurnOwner {
    #[default]
    Ordinary,
    Observation {
        wait: ContinuationId,
        lease: ParkedObservationLease,
    },
    ReturningCaught {
        completed_wait: ContinuationId,
    },
}
/// Created only by the daemon's real request replacement, never by an RPC.
/// The original positive polling request remains the zero-effect read witness.
#[derive(Clone, Debug)]
struct ReadyPolledRead {
    request: Ivar<SchedRequest>,
    response: Ivar<SchedResponse>,
    origin: ResourceOrigin,
    epoch: u64,
    original: Resources,
}
impl ReadyPolledRead {
    fn matches(&self, turn: &ThreadNextTurn, resources: &Resources) -> bool {
        let mut expected = self.original.clone();
        expected.poll_attempt = 0;
        self.original.poll_attempt > 0
            && self.request == turn.req
            && self.response == turn.resp
            && Some(self.origin) == turn.protocol.origin
            && self.epoch == turn.protocol.epoch
            && expected == *resources
    }
}
#[derive(Clone, Debug, Default)]
pub(crate) struct TurnProtocol {
    pub epoch: u64,
    pub owner: NextTurnOwner,
    pub origin: Option<ResourceOrigin>,
    ready_read: Option<ReadyPolledRead>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestKey {
    pub dettid: DetTid,
    pub mm: MmId,
    pub epoch: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AlarmControl {
    pub continuation: ContinuationId,
    pub request: RequestKey,
    pub site: CallbackSignalSite,
    pub lease: ParkedObservationLease,
    pub permit: SignalDeliveryPermit,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResumeTicket {
    pub continuation: ContinuationId,
    pub cycle: u64,
    pub next_epoch: u64,
    pub site: CallbackSignalSite,
    pub nonce: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ResourceReply {
    Grant(ResumeStatus),
    ObserveSignal(Box<AlarmControl>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ObservationFinish {
    ResumeSameWait,
    InterruptForCaught { selection: PreparedSignalToken },
    Terminate { selection: PreparedSignalToken },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FinishAck {
    AwaitResume(ResumeTicket),
    Interrupted,
    Terminate,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProtocolFailure {
    Identity,
    Phase,
    Unsupported,
    Overflow,
    UnexpectedControl,
    Timer(TimerFailure),
    Observation(reverie::SignalObservationFailure),
}
impl From<TimerFailure> for ProtocolFailure {
    fn from(value: TimerFailure) -> Self {
        Self::Timer(value)
    }
}
#[derive(Clone, Debug)]
enum SavedMembership {
    Timed(LogicalTime),
    Queued(SuspendedRunQueueEntry),
    ChildWait { parent: DetPid, spec: ChildWaitSpec },
}
#[derive(Clone, Copy, Debug)]
enum ContinuationPhase {
    Waiting,
    AwaitingResumeRegistration(ResumeTicket),
    Observing(ParkedObservationLease),
}
#[derive(Clone, Debug)]
pub(super) struct OwnedRequest {
    id: ContinuationId,
    request: Ivar<SchedRequest>,
    response: Ivar<SchedResponse>,
    resources: Resources,
    origin: ResourceOrigin,
    ready_read: Option<ReadyPolledRead>,
    membership: Option<SavedMembership>,
    cycle: u64,
    phase: ContinuationPhase,
}
#[derive(Clone, Debug)]
pub(crate) enum ControlIntent {
    Finish {
        wait: ContinuationId,
        lease: ParkedObservationLease,
        site: CallbackSignalSite,
        finish: ObservationFinish,
        ack: Ivar<Result<FinishAck, ProtocolFailure>>,
    },
    Resume {
        ticket: ResumeTicket,
        site: CallbackSignalSite,
        response: Ivar<SchedResponse>,
    },
}
#[derive(Debug, Default)]
pub(super) struct ParkedRequests {
    pub(super) nonce: u64,
    pub(super) requests: BTreeMap<ContinuationId, OwnedRequest>,
    intents: VecDeque<ControlIntent>,
    wake: Ivar<()>,
    pub failure: Option<ProtocolFailure>,
    pub control: Option<reverie::BackendSignalControl>,
    pub permits: BTreeMap<DetTid, SignalDeliveryPermit>,
    pub exit_fences: BTreeMap<DetTid, ExitBoundaryFence>,
    pub child_exit_reservations: BTreeMap<ProcessGeneration, ChildExitReservation>,
    pub completed_child_exits: BTreeMap<ProcessGeneration, CompletedChildExit>,
    /// Final processes without a live parent have no child-publication callback.
    /// Keep their descriptor cleanup fenced after the terminal boundary has
    /// cancelled peer RPCs, until the backend reports successful retirement.
    pub process_retirements: BTreeMap<ProcessGeneration, reverie::BackendProcessRetirement>,
    pub completed_process_retirements:
        BTreeMap<ProcessGeneration, reverie::BackendProcessRetirement>,
    pub terminal_processes: BTreeMap<ProcessGeneration, TerminalProcess>,
    pub completed: BTreeMap<DetTid, reverie::SignalBoundaryReceipt>,
    pub running: Option<DetTid>,
    pub failures: Vec<reverie::SignalProcessId>,
    pub failure_wakes: Vec<futures::channel::oneshot::Sender<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ExitBoundaryFence {
    pub permit: SignalDeliveryPermit,
    pub process: DetPid,
    pub mm: MmId,
    pub group: bool,
}

/// Exact backend process lifetime used as a deterministic map key.
///
/// `SignalProcessId` deliberately does not promise ordering.  The scheduler
/// needs ordered iteration for diagnostics and duplicate handling, so retain
/// the same identity in an orderable representation rather than keying on the
/// reusable numeric pid alone.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ProcessGeneration {
    pub pid: DetPid,
    pub generation: u64,
}

impl ProcessGeneration {
    pub(super) fn from_backend(process: SignalProcessId) -> Self {
        Self {
            pid: DetPid::from_raw(process.tgid.as_raw()),
            generation: process.generation,
        }
    }
}

/// Scheduler classification at the final logical process transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FinalProcessClass {
    Root,
    DirectParentTerminal { parent: SignalProcessId },
    LiveParent { parent: SignalProcessId },
}

/// Exact terminal process record retained after timer/task retirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TerminalProcess {
    pub process: SignalProcessId,
    pub class: FinalProcessClass,
    pub status: ExitStatus,
}

/// State of the distinct stage-2 child-publication transaction.
///
/// The exit-boundary permit above is stage 1.  This state survives its
/// consumption and keeps the daemon fenced until the exact-generation child
/// status has either committed (with a typed receipt), failed after commit
/// (with that same receipt and errno), or been rejected before mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChildExitReservationPhase {
    AwaitingTerminalStatus,
    AwaitingPublication {
        status: ExitStatus,
    },
    Published {
        completion: ChildExitCompletion,
        result: ChildExitPublicationResult,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ChildExitReservation {
    pub child: SignalProcessId,
    pub parent: SignalProcessId,
    pub phase: ChildExitReservationPhase,
}

/// Terminal stage-2 outcome for one exact child generation.
///
/// Retaining rejected errors as well as committed receipts makes every exact
/// duplicate terminal: it can be acknowledged without publishing again, and a
/// failed-after-commit operation can never be forwarded twice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CompletedChildExit {
    pub completion: ChildExitCompletion,
    pub result: ChildExitPublicationResult,
}

/// Preserve the owner of an operation before a recipient has been selected.
/// In particular, process-wide recipient discovery has no failed task identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SelectionFailure {
    pub pid: DetPid,
    pub tid: Option<DetTid>,
    pub failure: ProtocolFailure,
}

impl Scheduler {
    pub(crate) fn fail_parked(&mut self, tid: DetTid, failure: ProtocolFailure) {
        let pid = self
            .thread_tree
            .thread_to_leader
            .get(&tid)
            .copied()
            .unwrap_or(tid);
        self.fail_parked_location(pid, Some(tid), failure, "KVM parked signal protocol");
    }

    pub(super) fn fail_parked_selection(&mut self, error: SelectionFailure) {
        self.fail_parked_location(
            error.pid,
            error.tid,
            error.failure,
            if error.tid.is_some() {
                "KVM parked signal observation"
            } else {
                "KVM parked signal process selection"
            },
        );
    }

    fn fail_parked_location(
        &mut self,
        pid: DetPid,
        tid: Option<DetTid>,
        failure: ProtocolFailure,
        phase: &'static str,
    ) {
        if self.parked.failure.is_none() {
            tracing::error!(
                "KVM parked signal protocol failed for process {}, task {:?}: {:?}",
                pid,
                tid,
                failure
            );
            self.parked.failure = Some(failure);
        }
        self.real_timers.fail(
            pid,
            match failure {
                ProtocolFailure::Timer(f) => f,
                _ => TimerFailure::Unsupported,
            },
        );
        self.blocked
            .timed_waiters
            .remove_controlled_real_timer_deadline(pid);
        if let Some(wake) = self.report_backend_failure_location(super::BackendFailureLocation {
            pid: reverie::Pid::from_raw(pid.as_raw()),
            tid: tid.map(|tid| reverie::Tid::from_raw(tid.as_raw())),
            phase,
        }) {
            self.parked.failure_wakes.push(wake);
            self.parked.wake.try_put(());
        }
    }

    pub(crate) fn install_resource_origin(
        &mut self,
        tid: DetTid,
        origin: ResourceOrigin,
    ) -> Result<(), ProtocolFailure> {
        let turn = self.next_turns.get(&tid).ok_or(ProtocolFailure::Identity)?;
        if !self.rpc_incarnation_matches(tid, origin.mm) {
            return Err(ProtocolFailure::Identity);
        }
        // A control site exists only when the Reverie backend returned one, so
        // the site itself is the capability. `validate_site` still refuses a
        // site whose process and task were never bound to the real-timer table.
        if let Some(site) = origin.control.site() {
            let pid = *self
                .thread_tree
                .thread_to_leader
                .get(&tid)
                .ok_or(ProtocolFailure::Identity)?;
            self.real_timers.validate_site(pid, tid, origin.mm, site)?;
            match (origin.control, turn.protocol.owner) {
                (
                    ControlCapability::ParkedWait { .. }
                    | ControlCapability::PolledRead { .. }
                    | ControlCapability::ChildWait { .. },
                    NextTurnOwner::Ordinary | NextTurnOwner::ReturningCaught { .. },
                ) => {}
                (
                    ControlCapability::PublishOnly { lease, .. },
                    NextTurnOwner::Observation {
                        lease: expected, ..
                    },
                ) if lease == expected => {}
                _ => return Err(ProtocolFailure::Identity),
            }
        }
        let turn = self
            .next_turns
            .get_mut(&tid)
            .ok_or(ProtocolFailure::Identity)?;
        if turn.req.try_read().is_some() {
            // Existing legacy signal merging must retain its installed origin.
            if turn.protocol.origin != Some(origin) {
                return Err(ProtocolFailure::Phase);
            }
        } else {
            turn.protocol.origin = Some(origin);
            turn.protocol.ready_read = None;
        }
        Ok(())
    }

    pub(crate) fn post_control(
        &mut self,
        tid: DetTid,
        mm: MmId,
        intent: ControlIntent,
    ) -> Result<(), ProtocolFailure> {
        if !self.rpc_incarnation_matches(tid, mm) || self.backend_failed() {
            return Err(ProtocolFailure::Identity);
        }
        let expected_tid = match &intent {
            ControlIntent::Finish { wait, .. } => wait.dettid,
            ControlIntent::Resume { ticket, .. } => ticket.continuation.dettid,
        };
        if tid != expected_tid {
            return Err(ProtocolFailure::Identity);
        }
        self.parked.intents.push_back(intent);
        self.parked.wake.try_put(());
        Ok(())
    }

    pub(crate) fn registered_process(&self, tid: DetTid) -> Option<DetPid> {
        self.thread_tree.thread_to_leader.get(&tid).copied()
    }

    pub(crate) fn complete_signal_exec(
        &mut self,
        pid: DetPid,
        tid: DetTid,
        previous_mm: MmId,
        current_mm: MmId,
        identity: reverie::SignalTaskIdentity,
    ) -> Result<(), ProtocolFailure> {
        if self.registered_process(tid) != Some(pid)
            || self.parked.requests.keys().any(|wait| wait.dettid == tid)
        {
            return Err(ProtocolFailure::Identity);
        }
        self.real_timers
            .rebind_after_exec(pid, tid, previous_mm, current_mm, identity)?;
        self.exec_incarnations.insert(tid, current_mm);
        Ok(())
    }

    pub(crate) fn parked_resume_resources(
        &self,
        ticket: ResumeTicket,
        tid: DetTid,
        mm: MmId,
        site: CallbackSignalSite,
    ) -> Result<(DetPid, Resources), ProtocolFailure> {
        let owned = self
            .parked
            .requests
            .get(&ticket.continuation)
            .ok_or(ProtocolFailure::Identity)?;
        if owned.id.dettid != tid
            || owned.origin.mm != mm
            || ticket.site != site
            || !matches!(owned.phase, ContinuationPhase::AwaitingResumeRegistration(expected) if expected == ticket)
        {
            return Err(ProtocolFailure::Identity);
        }
        Ok((
            *self
                .thread_tree
                .thread_to_leader
                .get(&tid)
                .ok_or(ProtocolFailure::Identity)?,
            owned.resources.clone(),
        ))
    }

    pub(crate) fn apply_signal_dequeue(
        &mut self,
        pid: DetPid,
        dequeue: reverie::SignalDequeue,
        now: LogicalTime,
    ) -> Result<super::real_timer::DequeueAck, TimerFailure> {
        if self.backend_failed() {
            self.real_timers.fail(pid, TimerFailure::Unsupported);
        }
        let (ack, next) = self.real_timers.dequeue(pid, dequeue, now)?;
        if let Some(deadline) = next {
            self.blocked
                .timed_waiters
                .insert_controlled_real_timer_deadline(deadline, pid);
        }
        Ok(ack)
    }

    /// Replaces the process's `ITIMER_REAL`.
    ///
    /// The installed control decides which timer model serves it: with a
    /// control, the real timer follows the backend's acknowledged dequeues;
    /// without one, Detcore's own alarm model in the timed waiters serves it.
    /// A backend whose guest is reached only through the control never runs a
    /// sequentialized guest without one: Detcore reports that need through
    /// [`reverie::Tool::requires_signal_control`], and reverie-kvm refuses the
    /// run before any guest code, with ENOSYS from the runner that has no
    /// control to offer and EINVAL from a runner whose offer was not taken.
    pub(crate) fn replace_real_timer(
        &mut self,
        pid: DetPid,
        tid: DetTid,
        now: LogicalTime,
        duration: LogicalTime,
        interval: LogicalTime,
        signal: nix::sys::signal::Signal,
    ) -> Result<(LogicalTime, LogicalTime), TimerFailure> {
        if !self.signal_control_installed() {
            return Ok(self.register_alarm(pid, tid, now, duration, interval, signal));
        }
        if signal != nix::sys::signal::Signal::SIGALRM {
            return Err(TimerFailure::Unsupported);
        }
        let (old, next) = self
            .real_timers
            .replace(pid, tid, now, duration, interval)?;
        self.blocked
            .timed_waiters
            .remove_controlled_real_timer_deadline(pid);
        if let Some(deadline) = next {
            self.blocked
                .timed_waiters
                .insert_controlled_real_timer_deadline(deadline, pid);
        }
        Ok((old.remaining, old.interval))
    }

    pub(crate) fn itimer_snapshot(
        &self,
        pid: DetPid,
        now: LogicalTime,
    ) -> Result<super::real_timer::ItimerSnapshot, TimerFailure> {
        if self.signal_control_installed() {
            return self.real_timers.snapshot(pid, now);
        }
        Ok(match self.blocked.timed_waiters.alarm_state(pid) {
            Some((_, interval)) => super::real_timer::ItimerSnapshot {
                remaining: self.alarm_remaining(pid, now),
                interval,
            },
            None => super::real_timer::ItimerSnapshot::default(),
        })
    }

    pub(super) fn control_waiter(&self) -> Ivar<()> {
        self.parked.wake.clone()
    }

    pub(super) fn wake_control_waiter(&mut self) {
        self.parked.wake.try_put(());
    }

    pub(super) fn control_barrier(&self) -> bool {
        !self.parked.exit_fences.is_empty()
            || !self.parked.child_exit_reservations.is_empty()
            || !self.parked.process_retirements.is_empty()
            || self
                .parked
                .requests
                .values()
                .any(|r| matches!(r.phase, ContinuationPhase::AwaitingResumeRegistration(_)))
    }

    /// Both timed pop sites dispatch before any host-capable signal operation.
    pub(super) fn dispatch_timed_signal(
        &mut self,
        deadline: LogicalTime,
        id: SignalTimerId,
        tid: DetTid,
        signal: timed_waiters::TimerSignal,
        normal_due: bool,
        observed_now: LogicalTime,
    ) {
        let signal = match signal {
            timed_waiters::TimerSignal::Send(signal) => signal,
            // Detcore cannot deliver real-time signals. An expiration Linux
            // discards, or queues where nothing can yet observe it, changes
            // nothing, and the run goes on; any other stops it by name
            // (https://github.com/rrnewton/hermit/issues/3893,
            // https://github.com/rrnewton/hermit/issues/4089). Only a fail-closed
            // run registers such an expiration (`Detcore::handle_timer_settime`);
            // a permissive one never arms it.
            timed_waiters::TimerSignal::RefuseRealTime(signo) => {
                match self.real_time_expiration_fate(id.process(), tid, signo) {
                    RealTimeFate::Discarded => return,
                    RealTimeFate::Queued => {
                        *self
                            .pending_real_time_expirations
                            .entry(id.process())
                            .or_default() |= kernel_signal_bit(signo);
                        super::note_real_time_expiration_kept_pending();
                        tracing::debug!(
                            "[scheduler] real-time signal {} of a timer of process {} expired while every thread blocked it; pending",
                            signo,
                            id.process()
                        );
                        return;
                    }
                    RealTimeFate::Deliverable => super::refuse_real_time_expiration(signo),
                }
            }
        };
        if self.signal_control_installed() && matches!(id, SignalTimerId::Alarm(_)) {
            if let Err(failure) = self.publish_real_expiry(id.process(), deadline) {
                self.fail_parked(tid, failure);
            }
        } else if normal_due && matches!(id, SignalTimerId::ChildExit { .. }) {
            let parent = id.process();
            self.blocked.sigchld_ready.insert(parent);
            if let Some(deferred) = self.deferred_child_exit_sigchld(parent, tid) {
                // That thread already dequeued a `SIGCHLD` of this process (the
                // kernel's own, raised when the tracer released the child), and
                // its delivery was deferred. A standard signal coalesces, so
                // that delivery stands for this exit: commit it now instead of
                // sending a second one, which would also find the thread in no
                // pool at all.
                self.blocked.sigchld_deferred.remove(&deferred);
                self.blocked.sigchld_ready.insert(deferred);
                self.run_queue.push_eager_io_repoll(deferred);
                if let SignalTimerId::ChildExit { child, .. } = id {
                    self.child_exit_sigchld.stand_in(parent, child);
                }
            } else {
                // Where the scheduler models signal targets, send to the thread
                // the kernel gives this shared-queue `SIGCHLD`: the child's
                // creator (`tid`) if it does not block the signal, otherwise the
                // next thread that does not. Only if that thread sleeps outside
                // the schedule is it woken (`arm_signaled_background`); a gated
                // wait holds `SIGCHLD` until its call returns
                // (https://github.com/rrnewton/hermit/issues/3146).
                let target = if self.models_signal_targets {
                    self.kernel_sigchld_target(parent, tid).unwrap_or(tid)
                } else {
                    tid
                };
                let SignalTimerId::ChildExit { child, .. } = id else {
                    unreachable!("matched a ChildExit timer");
                };
                if !self.send_child_exit_sigchld(parent, target, child) {
                    self.fire_alarm(parent, target, signal);
                }
            }
        } else if !self.divert_in_guest_fatal_alarm(deadline, id, tid, signal, observed_now) {
            self.fire_alarm(id.process(), tid, signal);
        }
    }

    /// What Linux does with an expiration of a timer of `process` (armed by
    /// `tid`) that notifies with real-time signal `signo`, as far as the guest
    /// could tell (https://github.com/rrnewton/hermit/issues/4089):
    ///
    /// - [`RealTimeFate::Discarded`]: nobody can see it. The process has no
    ///   live thread, or the signal is ignored and a thread that does not block
    ///   it takes it, which discards it at delivery (or at generation,
    ///   `sig_ignored`).
    /// - [`RealTimeFate::Queued`]: every live thread blocks it, so Linux queues
    ///   it, and nothing can observe it yet: no signalfd the process created or
    ///   inherited reads it (`signalfd_masks`), no handler return may unblock it
    ///   (`handler_restore_unblocked`), and no thread of the process has made a
    ///   call outside the schedule (`background_call_processes`). Kept in
    ///   `pending_real_time_expirations`.
    /// - [`RealTimeFate::Deliverable`]: anything else, including whatever this
    ///   cannot decide; the run refuses as before.
    ///
    /// Each mask is the one the schedule fixes (`thread_signal_mask`, or the
    /// guest's own mask for a thread Detcore holds under a private one,
    /// `saved_guest_sigmasks`). A thread in a call outside the schedule makes
    /// the expiration deliverable: its live mask is host-timed, and its
    /// recorded one is the call's temporary mask, while the mask restored
    /// when the call returns may not block the signal. The dispositions are
    /// read from a thread stopped at a request. Called only at the two timed
    /// pops, where no guest thread holds the turn.
    ///
    /// Without signal-target modelling (`models_signal_targets`) none of those
    /// masks is recorded, and the arming thread's state decides as before.
    pub(super) fn real_time_expiration_fate(
        &self,
        process: DetPid,
        tid: DetTid,
        signo: i32,
    ) -> RealTimeFate {
        let bit = kernel_signal_bit(signo);
        if !self.models_signal_targets {
            return match self.read_sleeper_signal_state(tid) {
                Ok(state) if state.discarded_at_generation() & bit != 0 => RealTimeFate::Discarded,
                Err(SleeperStateUnreadable::Gone) => RealTimeFate::Discarded,
                _ => RealTimeFate::Deliverable,
            };
        }
        let threads = self.process_signal_targets(process);
        if threads.is_empty() {
            return RealTimeFate::Discarded;
        }
        let mut every_thread_blocks = true;
        let mut outside_the_schedule = false;
        let mut ignored = None;
        for &thread in &threads {
            if self.blocked.out_of_scheduler_masks.contains_key(&thread) {
                outside_the_schedule = true;
                continue;
            }
            let mask = match self.saved_guest_sigmasks.get(&thread) {
                Some(mask) => *mask,
                None => match self.thread_signal_mask(thread) {
                    Some(mask) => mask,
                    None => return RealTimeFate::Deliverable,
                },
            };
            if mask & bit == 0 {
                every_thread_blocks = false;
            }
            if ignored.is_none() {
                match self.read_sleeper_signal_state(thread) {
                    Ok(state) => ignored = Some(state.ignored & bit != 0),
                    Err(_) => return RealTimeFate::Deliverable,
                }
            }
        }
        let Some(ignored) = ignored else {
            // Every thread is outside the schedule.
            return RealTimeFate::Deliverable;
        };
        if ignored && !every_thread_blocks {
            return RealTimeFate::Discarded;
        }
        let recorded =
            |map: &BTreeMap<DetPid, u64>| map.get(&process).is_some_and(|mask| mask & bit != 0);
        if outside_the_schedule
            || !every_thread_blocks
            || self.background_call_processes.contains(&process)
            || recorded(&self.signalfd_masks)
            || recorded(&self.handler_restore_unblocked)
        {
            return RealTimeFate::Deliverable;
        }
        RealTimeFate::Queued
    }

    /// A new process inherits what its creator recorded about signalfds and
    /// handler returns (`signalfd_masks`, `handler_restore_unblocked`): a fork
    /// copies the descriptor table, and a fork inside a handler returns from it
    /// in the child too. Copied at creation, so a later re-parenting changes
    /// nothing.
    pub(crate) fn inherit_real_time_records(&mut self, creator: DetPid, child: DetPid) {
        for map in [
            &mut self.signalfd_masks,
            &mut self.handler_restore_unblocked,
        ] {
            if let Some(mask) = map.get(&creator).copied() {
                *map.entry(child).or_default() |= mask;
            }
        }
    }

    /// Record that a handler's return in `process` restores a mask leaving
    /// `unblocked` (real-time signals, a kernel sigset) unblocked
    /// (`handler_restore_unblocked`).
    pub(crate) fn record_handler_restore_unblocked(&mut self, process: DetPid, unblocked: u64) {
        *self.handler_restore_unblocked.entry(process).or_default() |= unblocked;
    }

    /// Record that `process` created a signalfd for `mask` (a kernel sigset),
    /// so a real-time timer expiration it could read is never kept pending
    /// (`real_time_expiration_fate`).
    pub(crate) fn register_signalfd(&mut self, process: DetPid, mask: u64) {
        *self.signalfd_masks.entry(process).or_default() |= mask;
    }

    /// The real-time signals pending for `process` from timer expirations
    /// that every thread blocked (`pending_real_time_expirations`).
    pub(crate) fn pending_real_time_expirations(&self, process: DetPid) -> u64 {
        self.pending_real_time_expirations
            .get(&process)
            .copied()
            .unwrap_or(0)
    }

    /// Discard `signals` from `process`'s pending real-time expirations, as
    /// Linux flushes a pending signal whose disposition becomes `SIG_IGN`.
    pub(crate) fn discard_pending_real_time_expirations(&mut self, process: DetPid, signals: u64) {
        if let Some(pending) = self.pending_real_time_expirations.get_mut(&process) {
            *pending &= !signals;
            if *pending == 0 {
                self.pending_real_time_expirations.remove(&process);
            }
        }
    }

    /// The thread of `process` whose deferred `SIGCHLD` delivery
    /// (`blocked.sigchld_deferred`) a child-exit timer of `process` commits.
    /// Where the scheduler models signal targets, any thread of the process may
    /// have dequeued the shared-queue signal: the child's creator (`creator`)
    /// first, as the kernel prefers it, then the others in thread-ID order.
    /// Otherwise the timer targets the leader, so only the leader is looked up,
    /// as before.
    fn deferred_child_exit_sigchld(&self, process: DetPid, creator: DetTid) -> Option<DetTid> {
        if !self.models_signal_targets {
            return self
                .blocked
                .sigchld_deferred
                .contains(&process)
                .then_some(process);
        }
        if self.blocked.sigchld_deferred.contains(&creator)
            && self.sigchld_process(creator) == process
        {
            return Some(creator);
        }
        self.blocked
            .sigchld_deferred
            .iter()
            .copied()
            .find(|thread| self.sigchld_process(*thread) == process)
    }

    fn publish_real_expiry(
        &mut self,
        pid: DetPid,
        deadline: LogicalTime,
    ) -> Result<(), ProtocolFailure> {
        let process = self.real_timers.process_identity(pid)?;
        let control = self
            .parked
            .control
            .clone()
            .ok_or(ProtocolFailure::Identity)?;
        let mut siginfo = [0; 128];
        siginfo[..4].copy_from_slice(&libc::SIGALRM.to_ne_bytes());
        siginfo[8..12].copy_from_slice(&libc::SI_KERNEL.to_ne_bytes());
        let event = SignalEvent::new(
            libc::SIGALRM,
            siginfo,
            SignalTarget::Process { pid: process.tgid },
        )
        .map_err(|_| ProtocolFailure::Identity)?;
        let expiry = self.real_timers.expire(pid, deadline)?;
        // No await/unlock between publication and consuming this exact causal
        // expiry. Standard-signal coalescing is not used as a retry mechanism.
        let outcome = control.process.publish_alarm(process, event);
        if matches!(
            outcome,
            ProcessSignalPublicationResult::FailedAfterCommit { .. }
        ) {
            self.parked.failures.push(process);
        }
        self.real_timers.publication(expiry, outcome)?;
        Ok(())
    }

    /// Called by the daemon only after the quiescent due-event drain. Backend
    /// membership is intersected with ordered scheduler admission and actual
    /// operation capabilities; host callback arrival is never a tie breaker.
    pub(super) fn select_parked_alarm(&mut self) -> Result<(), SelectionFailure> {
        let Some(control) = self.parked.control.clone() else {
            return Ok(());
        };
        for pid in self.real_timers.live_processes() {
            let process_failure = |failure| SelectionFailure {
                pid,
                tid: None,
                failure,
            };
            let process = self
                .real_timers
                .process_identity(pid)
                .map_err(|failure| process_failure(failure.into()))?;
            if self
                .parked
                .permits
                .values()
                .any(|p| p.task.process == process)
            {
                continue;
            }
            let recipients = control
                .process
                .alarm_recipients(process)
                .map_err(|_| process_failure(ProtocolFailure::Identity))?;
            for recipient in recipients {
                let tid = DetTid::from_raw(recipient.task.tid.as_raw());
                let Some((mm, identity)) = self.real_timers.task_identity(pid, tid) else {
                    continue;
                };
                if identity != recipient.task
                    || !self.rpc_incarnation_matches(tid, mm)
                    || self.thread_is_logically_killed(tid)
                    || self.parked.permits.contains_key(&tid)
                {
                    continue;
                }
                let Some(turn) = self.next_turns.get(&tid) else {
                    continue;
                };
                if turn.protocol.owner != NextTurnOwner::Ordinary {
                    continue;
                }
                let Some(origin) = turn.protocol.origin else {
                    continue;
                };
                let deadline = match origin.control {
                    ControlCapability::ParkedWait { policy, .. } => match policy {
                        ParkedWaitPolicy::NanosleepNoHandlerRestart { absolute_deadline } => {
                            absolute_deadline
                        }
                        ParkedWaitPolicy::PauseNoHandlerRestart => LogicalTime::INDEFINITE,
                    },
                    ControlCapability::PolledRead { .. } => LogicalTime::INDEFINITE,
                    ControlCapability::ChildWait { .. } => {
                        let Some(Ok(resources)) = turn.req.try_read() else {
                            continue;
                        };
                        let (parent, spec) = self
                            .validate_child_wait_resources(tid, &resources)
                            .map_err(|failure| SelectionFailure {
                                pid,
                                tid: Some(tid),
                                failure,
                            })?;
                        self.validate_membership(tid, Some((parent, spec)))
                            .map_err(|failure| SelectionFailure {
                                pid,
                                tid: Some(tid),
                                failure,
                            })?;
                        // Selection and readiness are tested under the same
                        // scheduler lock, before reserving/dequeuing a signal.
                        // A ready child or ECHILD wins this wait's completion.
                        if self.ready_child_wait(parent, spec).is_some()
                            || !self.has_child_wait_target(parent, spec)
                        {
                            // A selector change can leave a blocked waiter
                            // without a matching child. Admit its original
                            // request so ordinary completion can actually win.
                            if self.blocked.child_waiters.remove(&tid).is_some() {
                                self.runqueue_push_back(tid);
                            }
                            continue;
                        }
                        LogicalTime::INDEFINITE
                    }
                    _ => continue,
                };
                if deadline <= self.committed_time
                    || turn.req.try_read().is_none()
                    || turn.resp.try_read().is_some()
                {
                    continue;
                }
                // Other wait families remain pending for their existing return
                // boundary; this does not claim that their interruption works.
                self.begin_alarm_observation(pid, tid, recipient.task)
                    .map_err(|failure| SelectionFailure {
                        pid,
                        tid: Some(tid),
                        failure,
                    })?;
                break;
            }
        }
        Ok(())
    }

    fn validate_child_wait_resources(
        &self,
        tid: DetTid,
        resources: &Resources,
    ) -> Result<(DetPid, ChildWaitSpec), ProtocolFailure> {
        if resources.tid != tid || resources.poll_attempt != 0 || resources.resources.len() != 1 {
            return Err(ProtocolFailure::Unsupported);
        }
        let Some((ResourceID::WaitChild { parent, spec }, Permission::R)) =
            resources.resources.iter().next()
        else {
            return Err(ProtocolFailure::Unsupported);
        };
        if self.registered_process(tid) != Some(*parent)
            || spec.owner.is_some_and(|owner| owner != tid)
            || spec.exit_class != ChildWaitExitClass::Sigchld
        {
            return Err(ProtocolFailure::Identity);
        }
        Ok((*parent, *spec))
    }

    fn validate_membership(
        &self,
        tid: DetTid,
        child_wait: Option<(DetPid, ChildWaitSpec)>,
    ) -> Result<(), ProtocolFailure> {
        let timed = self.blocked.timed_waiters.thread_deadline(tid).is_some();
        let queued = self.run_queue.contains_tid(tid);
        if let Some(blocked) = self.blocked.child_waiters.get(&tid) {
            return if child_wait == Some(*blocked) && !timed && !queued {
                Ok(())
            } else {
                Err(ProtocolFailure::Phase)
            };
        }
        if child_wait.is_some() {
            // A child exit or another waiter can have just woken this request.
            return if queued && !timed {
                Ok(())
            } else {
                Err(ProtocolFailure::Phase)
            };
        }
        match (timed, queued) {
            (true, false) | (false, true) => Ok(()),
            (true, true) => Err(ProtocolFailure::Phase),
            (false, false) => Err(ProtocolFailure::Unsupported),
        }
    }

    /// The daemon validates membership before changing the timer phase. It
    /// holds exclusive scheduler access through this infallible removal.
    fn take_validated_membership(&mut self, tid: DetTid) -> SavedMembership {
        if let Some((parent, spec)) = self.blocked.child_waiters.remove(&tid) {
            return SavedMembership::ChildWait { parent, spec };
        }
        if let Some(deadline) = self.blocked.timed_waiters.thread_deadline(tid) {
            self.blocked.timed_waiters.remove(tid);
            return SavedMembership::Timed(deadline);
        }
        let priority = self.get_priority(tid);
        self.run_queue
            .suspend(tid, priority)
            .map(SavedMembership::Queued)
            .expect("validated scheduler membership")
    }

    fn begin_alarm_observation(
        &mut self,
        pid: DetPid,
        tid: DetTid,
        identity: reverie::SignalTaskIdentity,
    ) -> Result<(), ProtocolFailure> {
        let turn = self
            .next_turns
            .get(&tid)
            .cloned()
            .ok_or(ProtocolFailure::Identity)?;
        let origin = turn.protocol.origin.ok_or(ProtocolFailure::Unsupported)?;
        let site = origin.control.site().ok_or(ProtocolFailure::Unsupported)?;
        if !matches!(
            origin.rpc,
            RpcOrigin::DirectRequestResources | RpcOrigin::ResumeParkedRequest { .. }
        ) {
            return Err(ProtocolFailure::UnexpectedControl);
        }
        self.real_timers.validate_site(pid, tid, origin.mm, site)?;
        let resources = turn
            .req
            .try_read()
            .ok_or(ProtocolFailure::Phase)?
            .map_err(|_| ProtocolFailure::Phase)?;
        if turn.resp.try_read().is_some() {
            return Err(ProtocolFailure::Phase);
        }
        if let ControlCapability::ParkedWait { policy, .. } = origin.control {
            let expected = match policy {
                ParkedWaitPolicy::NanosleepNoHandlerRestart { absolute_deadline } => {
                    absolute_deadline
                }
                ParkedWaitPolicy::PauseNoHandlerRestart => LogicalTime::INDEFINITE,
            };
            if resources.resources.len() != 1
                || !resources
                    .resources
                    .contains_key(&ResourceID::SleepUntil(expected))
            {
                return Err(ProtocolFailure::Unsupported);
            }
        }
        if matches!(origin.control, ControlCapability::PolledRead { .. })
            && (resources.resources.len() != 1
                || resources.resources.get(&ResourceID::InternalIOPolling) != Some(&Permission::W)
                || (resources.poll_attempt == 0
                    && !turn
                        .protocol
                        .ready_read
                        .as_ref()
                        .is_some_and(|witness| witness.matches(&turn, &resources))))
        {
            return Err(ProtocolFailure::Unsupported);
        }
        let child_wait = if matches!(origin.control, ControlCapability::ChildWait { .. }) {
            Some(self.validate_child_wait_resources(tid, &resources)?)
        } else {
            None
        };
        let existing = self
            .parked
            .requests
            .iter()
            .find_map(|(id, r)| (r.request == turn.req).then_some(*id));
        let continuation = match existing {
            Some(id) => {
                let r = &self.parked.requests[&id];
                if !matches!(r.phase, ContinuationPhase::Waiting)
                    || r.response != turn.resp
                    || r.origin != origin
                {
                    return Err(ProtocolFailure::Identity);
                }
                id
            }
            None => ContinuationId {
                dettid: tid,
                nonce: self
                    .parked
                    .nonce
                    .checked_add(1)
                    .ok_or(ProtocolFailure::Overflow)?,
            },
        };
        // Publication consumes the old response. Reserve enough transport
        // space to acknowledge it and register the next response before any
        // timer or queue ownership changes.
        turn.protocol
            .epoch
            .checked_add(2)
            .ok_or(ProtocolFailure::Overflow)?;
        existing
            .map(|id| self.parked.requests[&id].cycle)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(ProtocolFailure::Overflow)?;
        self.parked
            .nonce
            .checked_add(if existing.is_some() { 1 } else { 2 })
            .ok_or(ProtocolFailure::Overflow)?;
        self.validate_membership(tid, child_wait)?;
        let lease = ParkedObservationLease {
            nonce: self
                .parked
                .nonce
                .checked_add(2)
                .ok_or(ProtocolFailure::Overflow)?,
        };
        let permit = SignalDeliveryPermit {
            task: identity,
            sequence: lease.nonce,
            site: Some(site),
        };
        self.parked
            .control
            .as_ref()
            .ok_or(ProtocolFailure::Identity)?
            .process
            .reserve_delivery(permit)
            .map_err(|_| ProtocolFailure::Identity)?;
        self.parked.permits.insert(tid, permit);
        if existing.is_none() {
            self.parked.nonce = continuation.nonce;
        }
        let membership = self.take_validated_membership(tid);
        let control = AlarmControl {
            continuation,
            request: RequestKey {
                dettid: tid,
                mm: origin.mm,
                epoch: turn.protocol.epoch,
            },
            site,
            lease,
            permit,
        };
        let cycle = existing
            .map(|id| self.parked.requests[&id].cycle)
            .unwrap_or(0);
        self.parked.requests.insert(
            continuation,
            OwnedRequest {
                id: continuation,
                request: turn.req,
                response: turn.resp.clone(),
                resources,
                origin,
                ready_read: turn.protocol.ready_read.clone(),
                membership: Some(membership),
                cycle,
                phase: ContinuationPhase::Observing(lease),
            },
        );
        self.parked.nonce = lease.nonce;
        let current = self
            .next_turns
            .get_mut(&tid)
            .expect("validated observation owner");
        current.protocol.epoch += 1; // checked before committing the permit
        current.protocol.owner = NextTurnOwner::Observation {
            wait: continuation,
            lease,
        };
        current.protocol.origin = None;
        current.protocol.ready_read = None;
        current.req = Ivar::new();
        current.resp = Ivar::new();
        self.runqueue_push_back(tid);
        turn.resp
            .put(SchedResponse::ObserveSignal(Box::new(control)));
        Ok(())
    }

    fn prepare_resume_ticket(
        &self,
        owned: &OwnedRequest,
        site: CallbackSignalSite,
    ) -> Result<ResumeTicket, ProtocolFailure> {
        let cycle = owned
            .cycle
            .checked_add(1)
            .ok_or(ProtocolFailure::Overflow)?;
        let epoch = self
            .next_turns
            .get(&owned.id.dettid)
            .ok_or(ProtocolFailure::Identity)?
            .protocol
            .epoch;
        Ok(ResumeTicket {
            continuation: owned.id,
            cycle,
            next_epoch: epoch.checked_add(1).ok_or(ProtocolFailure::Overflow)?,
            site,
            nonce: self
                .parked
                .nonce
                .checked_add(1)
                .ok_or(ProtocolFailure::Overflow)?,
        })
    }

    fn install_resume_ticket(&mut self, owned: &mut OwnedRequest, ticket: ResumeTicket) {
        self.parked.nonce = ticket.nonce;
        owned.cycle = ticket.cycle;
        owned.phase = ContinuationPhase::AwaitingResumeRegistration(ticket);
    }

    fn resume_ticket(
        &mut self,
        owned: &mut OwnedRequest,
        site: CallbackSignalSite,
    ) -> Result<ResumeTicket, ProtocolFailure> {
        let ticket = self.prepare_resume_ticket(owned, site)?;
        self.install_resume_ticket(owned, ticket);
        Ok(ticket)
    }

    fn finish_observation(
        &mut self,
        wait: ContinuationId,
        lease: ParkedObservationLease,
        site: CallbackSignalSite,
        finish: ObservationFinish,
    ) -> Result<FinishAck, ProtocolFailure> {
        let mut owned = self
            .parked
            .requests
            .remove(&wait)
            .ok_or(ProtocolFailure::Identity)?;
        let result = (|| {
            if !matches!(owned.phase, ContinuationPhase::Observing(expected) if expected == lease)
                || owned.origin.control.site() != Some(site)
            {
                return Err(ProtocolFailure::Identity);
            }
            let turn = self
                .next_turns
                .get(&wait.dettid)
                .ok_or(ProtocolFailure::Identity)?;
            if turn.protocol.owner != (NextTurnOwner::Observation { wait, lease })
                || turn.req.try_read().is_some()
                || turn.resp.try_read().is_some()
                || !self.run_queue.contains_tid(wait.dettid)
            {
                return Err(ProtocolFailure::Phase);
            }
            match finish {
                ObservationFinish::ResumeSameWait => {
                    let permit = self
                        .parked
                        .permits
                        .remove(&wait.dettid)
                        .ok_or(ProtocolFailure::Identity)?;
                    self.parked
                        .control
                        .as_ref()
                        .ok_or(ProtocolFailure::Identity)?
                        .process
                        .release_delivery(permit)
                        .map_err(|_| ProtocolFailure::Identity)?;
                    self.resume_ticket(&mut owned, site)
                        .map(FinishAck::AwaitResume)
                }
                ObservationFinish::InterruptForCaught { selection } => {
                    if selection.site != site {
                        return Err(ProtocolFailure::Identity);
                    }
                    let turn = self
                        .next_turns
                        .get_mut(&wait.dettid)
                        .ok_or(ProtocolFailure::Identity)?;
                    turn.protocol.epoch = turn
                        .protocol
                        .epoch
                        .checked_add(1)
                        .ok_or(ProtocolFailure::Overflow)?;
                    turn.protocol.owner = NextTurnOwner::ReturningCaught {
                        completed_wait: wait,
                    };
                    turn.protocol.origin = None;
                    turn.protocol.ready_read = None;
                    // This empty, queued gate protects the real remaining-time
                    // copyout, posthook and frame delivery. No synthetic turn.
                    Ok(FinishAck::Interrupted)
                }
                ObservationFinish::Terminate { selection } => {
                    if selection.site != site {
                        return Err(ProtocolFailure::Identity);
                    }
                    // Keep the existing execution gate until the driver's real
                    // signal exit performs ordinary logical retirement.
                    Ok(FinishAck::Terminate)
                }
            }
        })();
        if matches!(result, Ok(FinishAck::AwaitResume(_))) || result.is_err() {
            self.parked.requests.insert(wait, owned);
        }
        result
    }

    fn resume_request(
        &mut self,
        ticket: ResumeTicket,
        site: CallbackSignalSite,
        response: Ivar<SchedResponse>,
    ) -> Result<(), ProtocolFailure> {
        let mut owned = self
            .parked
            .requests
            .remove(&ticket.continuation)
            .ok_or(ProtocolFailure::Identity)?;
        let result = (|| {
            if !matches!(owned.phase, ContinuationPhase::AwaitingResumeRegistration(expected) if expected == ticket)
                || ticket.site != site
                || response.try_read().is_some()
            {
                return Err(ProtocolFailure::Identity);
            }
            self.real_timers.validate_site(
                *self
                    .thread_tree
                    .thread_to_leader
                    .get(&owned.id.dettid)
                    .ok_or(ProtocolFailure::Identity)?,
                owned.id.dettid,
                owned.origin.mm,
                site,
            )?;
            let tid = owned.id.dettid;
            let before = self.next_turns.get(&tid).ok_or(ProtocolFailure::Identity)?;
            if before.protocol.epoch.checked_add(1) != Some(ticket.next_epoch) {
                return Err(ProtocolFailure::Identity);
            }
            let owner = NextTurnOwner::Ordinary;
            let membership = owned.membership.take().ok_or(ProtocolFailure::Phase)?;
            match membership {
                SavedMembership::Timed(deadline) => {
                    self.run_queue.remove_tid(tid);
                    self.blocked.timed_waiters.insert(deadline, tid);
                }
                SavedMembership::Queued(entry) => {
                    let priority = self.get_priority(tid);
                    self.run_queue.suspend(tid, priority);
                    self.run_queue.restore(entry, priority);
                }
                SavedMembership::ChildWait { parent, spec } => {
                    if self.blocked.child_waiters.contains_key(&tid)
                        || self.blocked.timed_waiters.thread_deadline(tid).is_some()
                        || !self.run_queue.contains_tid(tid)
                    {
                        return Err(ProtocolFailure::Phase);
                    }
                    // Nested signal hooks can publish an exit, consume a child,
                    // or change group/owner matching while this waiter is an
                    // observation gate. Recheck the full selector before
                    // blocking again; that gate already supplies its wakeup.
                    if self.ready_child_wait(parent, spec).is_none()
                        && self.has_child_wait_target(parent, spec)
                    {
                        self.run_queue.remove_tid(tid);
                        self.blocked.child_waiters.insert(tid, (parent, spec));
                    }
                }
            }
            owned.origin.rpc = RpcOrigin::ResumeParkedRequest {
                continuation: owned.id,
                cycle: ticket.cycle,
            };
            let turn = self
                .next_turns
                .get_mut(&tid)
                .ok_or(ProtocolFailure::Identity)?;
            // Registration replaces only this response transport. Preserve the
            // daemon's ready-retry witness with the exact new transport/epoch.
            if let Some(witness) = owned.ready_read.as_mut() {
                witness.response = response.clone();
                witness.origin = owned.origin;
                witness.epoch = ticket.next_epoch;
            }
            turn.protocol.epoch = ticket.next_epoch;
            turn.protocol.origin = Some(owned.origin);
            turn.protocol.ready_read = owned.ready_read.clone();
            turn.protocol.owner = owner;
            turn.req = owned.request.clone();
            turn.resp = response.clone();
            owned.response = response;
            owned.phase = ContinuationPhase::Waiting;
            Ok(())
        })();
        self.parked.requests.insert(owned.id, owned);
        result
    }

    /// Called only by the daemon before tentative selection. An RPC may wake
    /// this boundary but never changes runqueue membership itself.
    pub(super) fn drain_control_intents(&mut self) {
        while let Some(intent) = self.parked.intents.pop_front() {
            let (tid, error) = match intent {
                ControlIntent::Finish {
                    wait,
                    lease,
                    site,
                    finish,
                    ack,
                } => {
                    let result = self.finish_observation(wait, lease, site, finish);
                    let error = result.as_ref().err().copied();
                    ack.put(result);
                    (wait.dettid, error)
                }
                ControlIntent::Resume {
                    ticket,
                    site,
                    response,
                } => (
                    ticket.continuation.dettid,
                    self.resume_request(ticket, site, response).err(),
                ),
            };
            if let Some(error) = error {
                self.fail_parked(tid, error);
                break;
            }
        }
        if self.parked.wake.try_read().is_some() {
            self.parked.wake = Ivar::new();
        }
    }

    pub(super) fn retire_parked_requests(&mut self, tid: DetTid) {
        // Cancellation is an ordinary lifecycle event. The runtime owns the
        // in-flight callback and consumes its permit before on_exit_thread.
        // A waiting operation with no selected effect can retire immediately.
        if !self.parked.permits.contains_key(&tid) {
            self.parked.requests.retain(|id, _| id.dettid != tid);
        }
        self.parked.wake.try_put(());
    }

    pub(super) fn clear_ready_polled_read(&mut self, tid: DetTid) {
        if let Some(turn) = self.next_turns.get_mut(&tid) {
            turn.protocol.ready_read = None;
        }
    }

    pub(super) fn settle_parked_grant(&mut self, tid: DetTid) {
        let Some(turn) = self.next_turns.get(&tid) else {
            return;
        };
        let req = turn.req.clone();
        self.parked.requests.retain(|_, owned| owned.request != req);
    }

    /// A real cross-task notification replaces a child-wait request with the
    /// legacy WaitidSignals grant. Its original capability cannot describe
    /// that replacement, and an ignored observation may have retained an owned
    /// request for the old transport. Retire only that exact waiting ownership.
    pub(super) fn retire_child_wait_signal_request(
        &mut self,
        tid: DetTid,
    ) -> Result<(), ProtocolFailure> {
        let Some(turn) = self.next_turns.get(&tid) else {
            return Ok(());
        };
        let Some(origin) = turn.protocol.origin else {
            return Ok(());
        };
        if !matches!(origin.control, ControlCapability::ChildWait { .. }) {
            return Ok(());
        }
        if turn.protocol.owner != NextTurnOwner::Ordinary
            || self.parked.permits.contains_key(&tid)
            || turn.resp.try_read().is_some()
        {
            return Err(ProtocolFailure::Phase);
        }
        let resources = turn
            .req
            .try_read()
            .ok_or(ProtocolFailure::Phase)?
            .map_err(|_| ProtocolFailure::Phase)?;
        let spec = self.validate_child_wait_resources(tid, &resources)?;
        if let Some(admission) = self.pending_run_queue_admissions.get(&tid) {
            // Signal notifications drain before queued admissions. A genuine
            // child wake can therefore be between its blocked and queued
            // states here; force_unblock_thread consumes this pending entry.
            if !matches!(admission, super::AdmitIntent::Fixed(super::AdmitSide::Back))
                || self.blocked.child_waiters.contains_key(&tid)
                || self.blocked.timed_waiters.thread_deadline(tid).is_some()
                || self.run_queue.contains_tid(tid)
            {
                return Err(ProtocolFailure::Phase);
            }
        } else {
            self.validate_membership(tid, Some(spec))?;
        }
        for owned in self
            .parked
            .requests
            .values()
            .filter(|owned| owned.request == turn.req)
        {
            if !matches!(owned.phase, ContinuationPhase::Waiting)
                || owned.response != turn.resp
                || owned.origin != origin
            {
                return Err(ProtocolFailure::Identity);
            }
        }
        self.settle_parked_grant(tid);
        let turn = self
            .next_turns
            .get_mut(&tid)
            .expect("validated child-wait owner");
        turn.protocol
            .origin
            .as_mut()
            .expect("validated child-wait origin")
            .control = ControlCapability::None;
        Ok(())
    }

    /// Normal polling replaces a request without completing its logical
    /// operation. Match the exact request, so a hook on the same task cannot
    /// change the suspended outer wait's ownership or resources.
    pub(super) fn rebind_parked_request(
        &mut self,
        tid: DetTid,
        previous: &Ivar<SchedRequest>,
        replacement: &Ivar<SchedRequest>,
        original: &Resources,
        resources: &Resources,
    ) {
        let turn = self
            .next_turns
            .get_mut(&tid)
            .expect("promoted request owner");
        let witness = turn.protocol.origin.and_then(|origin| {
            (matches!(origin.control, ControlCapability::PolledRead { .. })
                && matches!(
                    origin.rpc,
                    RpcOrigin::DirectRequestResources | RpcOrigin::ResumeParkedRequest { .. }
                )
                && original.poll_attempt > 0
                && original.resources.len() == 1
                && original.resources.get(&ResourceID::InternalIOPolling) == Some(&Permission::W)
                && matches!(previous.try_read(), Some(Ok(ref prior)) if prior == original)
                && turn.req == *replacement
                && turn.resp.try_read().is_none())
            .then(|| ReadyPolledRead {
                request: replacement.clone(),
                response: turn.resp.clone(),
                origin,
                epoch: turn.protocol.epoch,
                original: original.clone(),
            })
        });
        turn.protocol.ready_read = witness.clone();
        for owned in self.parked.requests.values_mut() {
            if owned.request == *previous {
                owned.request = replacement.clone();
                owned.resources = resources.clone();
                owned.ready_read = witness.clone();
            }
        }
    }
}
