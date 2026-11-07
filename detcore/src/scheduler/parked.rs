/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A logical parked request can use several single-use RPC responses. Only the
//! scheduler daemon moves its queue entry; RPC handlers post bounded intents.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;

use reverie::CallbackSignalSite;
use reverie::ChildExitCompletion;
use reverie::ChildExitPublicationResult;
use reverie::ExitStatus;
use reverie::ParkedObservationLease;
use reverie::PendingDomain;
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
/// How a thread can take a SIGALRM pending in its process's shared queue, if
/// it is the thread Linux's `complete_signal` chooses, read from the
/// scheduler's committed state (`Scheduler::alarm_reach`,
/// <https://github.com/rrnewton/hermit/issues/4929>).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AlarmReach {
    /// Parked in a wait the observation protocol interrupts (a `pause`, a
    /// `nanosleep`, a polled read, a child wait): it is observed.
    Parked,
    /// Polling a call whose probe never ends for a pending signal (`poll`, an
    /// unmasked `ppoll`, `epoll_wait`, a receive, `rt_sigtimedwait`): it is
    /// woken (`Scheduler::wake_polling_alarm_receiver`).
    Poll,
    /// Polling a call whose probe takes a pending signal itself, under the
    /// call's own temporary mask (`select`, `pselect6`, and a `ppoll` with a
    /// temporary mask, whose requests carry the EINTR marker, as a `poll`'s
    /// and an unmasked `ppoll`'s also do): its next probe takes the copy.
    TakesAtItsProbe,
    /// Queued to run with a request filed: step 3 resumes it first.
    Queued,
    /// A vfork parent: Linux cannot wake it either, so the copy waits for it.
    VforkParent,
    /// Parked in an `rt_sigsuspend` the backend's own call serves
    /// (`Scheduler::backend_suspend_mask`, SIGSHARE S2b): the step-2 drain's
    /// `wake_backend_signal_waits` ends that wait for a signal its temporary
    /// mask lets in, after this generation, as Linux's interruptible sleep
    /// under the call's mask ends (`sys_rt_sigsuspend`). Nothing is done here.
    BackendSuspend,
    /// Waiting where this backend cannot interrupt the wait: a futex wait, a
    /// call outside the schedule, or another wait outside the run queue that
    /// neither the observation protocol nor the step-2 drain ends (not a
    /// backend-served `rt_sigsuspend`, `BackendSuspend`). Passed over (D1).
    OutOfReach,
}
/// Which threads a SIGALRM receiver choice counts as able to take the copy
/// (`Scheduler::alarm_wants`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AlarmReachRule {
    /// Those this backend can give it to: the choice that is made.
    Backend,
    /// Also those waiting where this backend cannot interrupt the wait, which
    /// Linux wakes for the signal: the choice Linux makes, computed only to
    /// name where it differs (D1, `Scheduler::note_alarm_generation`).
    Linux,
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
        let expected = self.original.upgraded_to_runnable();
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
    /// The observation selected a caught signal: the syscall is interrupted.
    /// `dequeue` is the removal that selection made, as the observation's
    /// `Caught` step names it, so the scheduler can tell whether the signal
    /// that interrupts the wait is a timer's publication
    /// (`ParkedRequests::observed_timer_dequeues`).
    InterruptForCaught {
        selection: PreparedSignalToken,
        dequeue: Option<reverie::DequeueId>,
    },
    Terminate {
        selection: PreparedSignalToken,
    },
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
    /// Each thread's outstanding delivery permit, at most one per thread.
    /// Threads of one process hold theirs independently, as Linux delivers
    /// each thread's signals on its own return to user mode.
    pub permits: BTreeMap<DetTid, SignalDeliveryPermit>,
    /// Per process, the queued thread that is to take the SIGALRM pending in
    /// its shared queue, as Linux's `complete_signal` chooses it
    /// (`Scheduler::select_parked_alarm`); step 3 resumes it before any other
    /// (`Scheduler::first_alarm_taker_entry`). Derived anew at every selection
    /// pass.
    pub alarm_takers: BTreeMap<DetPid, DetTid>,
    /// The process whose alarm taker step 3 last resumed by alarm priority,
    /// with no selection of the run queue's own since. While it is set, step
    /// 3 resumes no alarm taker by that priority, and no polling receiver is
    /// woken, so the run queue makes at least one choice of its own between
    /// any two alarm grants, for the same process or another
    /// (`Scheduler::alarm_priority_available`,
    /// <https://github.com/rrnewton/hermit/issues/4929>).
    pub alarm_priority_served: Option<DetPid>,
    /// The pollers `Scheduler::wake_polling_alarm_receiver` woke, until step 3
    /// next resumes each: their substituted request is a signal delivery the
    /// alarm priority still serves (`Scheduler::takes_alarm_priority`).
    pub alarm_woken: BTreeSet<DetTid>,
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
    /// Timer expirations published through the control and not yet dequeued,
    /// with the time each was due: the instant Linux sends the signal
    /// (`publish_real_expiry`, `publish_posix_timer_expiry`, through
    /// `note_timer_publication`). A coalesced publication, whose standard
    /// signal was already pending, keeps the pending event's entry and its
    /// first due time, and adds none.
    pub(super) timer_publications: Vec<TimerPublication>,
    /// For a thread observing a parked wait, the removal of a timer's
    /// published signal that its observation made, with the timer's due time
    /// (`Scheduler::apply_signal_dequeue`). The observation's finish takes it:
    /// a caught interruption of a `nanosleep` by exactly that removal notes the
    /// due time for the sleep's remaining time
    /// (https://github.com/rrnewton/hermit/issues/4931).
    pub(super) observed_timer_dequeues: BTreeMap<DetTid, ObservedTimerDequeue>,
    /// The parent-death signals (a kernel sigset) Detcore published to each
    /// live process through the control (`publish_parent_death_signal`),
    /// which `select_parked_alarm` asks the backend's recipients for, beside
    /// `SIGALRM`, so that one interrupts a parked wait as a timer's does.
    /// Kept until the process's last thread leaves: a signal no longer
    /// pending has no recipients.
    pub published_parent_death_signals: BTreeMap<DetPid, u64>,
    /// The exits whose orphans a placing backend has been asked to place
    /// (`Scheduler::place_orphans_in_backend`), each once.
    pub placed_exits: BTreeSet<ProcessGeneration>,
}

/// A timer expiration published through the control and not yet dequeued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TimerPublication {
    pub pid: DetPid,
    pub process: SignalProcessId,
    pub event: SignalEvent,
    pub due: LogicalTime,
}

/// The removal of a timer's published signal by an observing thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ObservedTimerDequeue {
    pub wait: ContinuationId,
    pub lease: ParkedObservationLease,
    pub dequeue: reverie::DequeueId,
    pub due: LogicalTime,
}

impl ParkedRequests {
    /// Records a timer publication due at `due`, from the backend's receipt.
    ///
    /// A publication that installed its event (`coalesced` false) is recorded
    /// with its due time, replacing an identical event's entry, which can only
    /// be stale since no identical event was pending. A coalesced one
    /// installed nothing: the kernel keeps the first pending event, so an
    /// identical entry keeps its first due time, and with none tracked (the
    /// pending signal is not a timer's this ledger saw) no entry is made.
    /// A publication that did not commit records nothing.
    fn note_timer_publication(
        &mut self,
        pid: DetPid,
        process: SignalProcessId,
        event: SignalEvent,
        due: LogicalTime,
        outcome: &ProcessSignalPublicationResult,
    ) {
        let ProcessSignalPublicationResult::Committed(receipt) = outcome else {
            return;
        };
        if receipt.coalesced {
            return;
        }
        self.timer_publications
            .retain(|p| !(p.pid == pid && p.process == process && p.event == event));
        self.timer_publications.push(TimerPublication {
            pid,
            process,
            event,
            due,
        });
    }

    /// Drops a retired process's undequeued publications.
    pub(super) fn forget_timer_publications(&mut self, pid: DetPid) {
        self.timer_publications.retain(|p| p.pid != pid);
    }
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

/// Test only: the controlled turn loop's pause at an open observation gate
/// (`Scheduler::observation_gate_pause_due`). It sleeps in 1 ms steps until
/// `release` is set, for at most 10 s. A plain `std::thread::sleep` keeps it
/// free of thread-local state; the caller holds no scheduler lock.
#[cfg(test)]
pub(super) fn pause_until_released(release: &std::sync::atomic::AtomicBool) {
    for _ in 0..10_000 {
        if release.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
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
        if turn.req.is_full() {
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

    /// Acknowledges the backend's dequeue of a pending signal of `pid`. A
    /// dequeue of the process's `ITIMER_REAL` copy does not restart the timer
    /// here: the restart waits for the next reconcile point
    /// (`reconcile_controlled_real_timer`), where the traced path restarts its
    /// own (<https://github.com/rrnewton/hermit/issues/4929>).
    pub(crate) fn apply_signal_dequeue(
        &mut self,
        pid: DetPid,
        tid: DetTid,
        dequeue: reverie::SignalDequeue,
    ) -> Result<super::real_timer::DequeueAck, TimerFailure> {
        if self.backend_failed() {
            self.real_timers.fail(pid, TimerFailure::Unsupported);
        }
        let ack = self.real_timers.dequeue(pid, dequeue)?;
        // The removal of a timer's published signal from the process queue.
        // Made by a thread observing a parked wait, it may be the signal that
        // interrupts that wait; the observation's finish decides
        // (`finish_observation`).
        if dequeue.domain == PendingDomain::Process
            && let Some(index) = self.parked.timer_publications.iter().position(|p| {
                p.pid == pid && p.process == dequeue.process && p.event == dequeue.event
            })
        {
            let publication = self.parked.timer_publications.remove(index);
            if let Some(NextTurnOwner::Observation { wait, lease }) =
                self.next_turns.get(&tid).map(|turn| turn.protocol.owner)
            {
                self.parked.observed_timer_dequeues.insert(
                    tid,
                    ObservedTimerDequeue {
                        wait,
                        lease,
                        dequeue: dequeue.id(),
                        due: publication.due,
                    },
                );
            }
        }
        Ok(ack)
    }

    /// Restarts `pid`'s controlled real timer at reconcile point `t` if its
    /// last copy was dequeued, on its grid after `t`, and queues the new
    /// deadline. The reconcile points are the traced path's
    /// (`reconcile_alarm`, `reconcile_inactive_alarms`): the process's
    /// `getitimer` or `setitimer` (O1), the end of a signalfd read or
    /// `rt_sigtimedwait` that may have consumed the copy (O2), each at the
    /// caller's current time, and every quiesced boundary (O3), at the
    /// committed time. So a copy taken at a return to user mode restarts
    /// the timer after the turn that took it has been charged, as on the
    /// traced path, rather than at the backend's earlier dequeue instant.
    /// Returns the new deadline. A restart that does not fit fails the run.
    pub(super) fn reconcile_controlled_real_timer(
        &mut self,
        pid: DetPid,
        t: LogicalTime,
    ) -> Option<LogicalTime> {
        match self.real_timers.reconcile(pid, t) {
            Ok(Some(deadline)) => {
                self.blocked
                    .timed_waiters
                    .insert_controlled_real_timer_deadline(deadline, pid);
                tracing::debug!(
                    "[dpid {}] SIGALRM left the shared queue; ITIMER_REAL restarted at {:?}.",
                    pid,
                    deadline
                );
                Some(deadline)
            }
            Ok(None) => None,
            Err(failure) => {
                self.fail_parked_location(
                    pid,
                    None,
                    ProtocolFailure::Timer(failure),
                    "KVM controlled ITIMER_REAL restart",
                );
                None
            }
        }
    }

    /// O3 for the controlled real timers: restart, at committed time `t`,
    /// every one whose copy was dequeued since the last reconcile point.
    pub(super) fn reconcile_dequeued_real_timers(&mut self, t: LogicalTime) {
        for pid in self.real_timers.dequeued_processes() {
            self.reconcile_controlled_real_timer(pid, t);
        }
    }

    /// The period of `pid`'s controlled real timer, when it is periodic.
    pub(super) fn controlled_real_timer_period(&self, pid: DetPid) -> Option<LogicalTime> {
        self.real_timers.interval(pid)
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

    /// The release flag of the test-only `observation_gate_pause`, while it is
    /// not yet released and a parked observation is open: some thread's turn
    /// is still owned by its observation, whose finish is not yet drained.
    #[cfg(test)]
    pub(super) fn observation_gate_pause_due(
        &self,
    ) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        let release = self.observation_gate_pause.as_ref()?;
        let open = self
            .next_turns
            .values()
            .any(|turn| matches!(turn.protocol.owner, NextTurnOwner::Observation { .. }));
        (open && !release.load(std::sync::atomic::Ordering::Acquire)).then(|| release.clone())
    }

    /// Both timed pop sites dispatch before any host-capable signal operation.
    /// While it runs, `signal_sent_at` is the event's own time: a pass pops
    /// one due event, so committed time may already be past later deadlines,
    /// and a sleep the signal reaches is judged by when the signal was due
    /// (`wake_signaled_guest_as`).
    pub(super) fn dispatch_timed_signal(
        &mut self,
        deadline: LogicalTime,
        id: SignalTimerId,
        target: timed_waiters::SignalTarget,
        signal: timed_waiters::TimerSignal,
        normal_due: bool,
        observed_now: LogicalTime,
    ) {
        let previous = self.signal_sent_at.replace(deadline);
        self.dispatch_timed_signal_at(deadline, id, target, signal, normal_due, observed_now);
        self.signal_sent_at = previous;
    }

    fn dispatch_timed_signal_at(
        &mut self,
        deadline: LogicalTime,
        id: SignalTimerId,
        target: timed_waiters::SignalTarget,
        signal: timed_waiters::TimerSignal,
        normal_due: bool,
        observed_now: LogicalTime,
    ) {
        let tid = target.tid();
        let signal = match signal {
            timed_waiters::TimerSignal::Send(signal) => signal,
            // Detcore does not deliver a timer's real-time signal. An expiration Linux
            // discards, or queues where nothing can yet observe it, changes
            // nothing, and the run goes on; any other stops it by name
            // (https://github.com/rrnewton/hermit/issues/3893,
            // https://github.com/rrnewton/hermit/issues/4089). Only a fail-closed
            // run registers such an expiration (`Detcore::handle_timer_settime`);
            // a permissive one never arms it.
            timed_waiters::TimerSignal::RefuseRealTime(signo) => {
                match self.real_time_expiration_fate(id.process(), target, signo) {
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
        if let timed_waiters::SignalTarget::Thread(thread) = target {
            if matches!(id, SignalTimerId::Posix(..)) {
                self.fire_posix_timer(deadline, id, target, signal);
            } else {
                self.fire_thread_timer(id, thread, signal);
            }
        } else if self.signal_control_installed() && matches!(id, SignalTimerId::Alarm(_)) {
            if let Err(failure) = self.publish_real_expiry(id.process(), deadline) {
                self.fail_parked(tid, failure);
            }
        } else if self.signal_control_installed()
            && let SignalTimerId::Posix(process, timer) = id
        {
            // A controlled backend's guest tasks are reached only through the
            // control: the physical send below would hit a host process.
            if let Err(failure) = self.publish_posix_timer_expiry(process, timer, signal) {
                self.fail_parked(tid, failure);
            }
        } else if normal_due && matches!(id, SignalTimerId::ChildExit { .. }) {
            let parent = id.process();
            // Where the scheduler models signal targets, the thread that takes
            // this send is marked when it does (`dequeue_sigchld`); a mark for
            // the process, its leader's id, would let any thread's delivery of
            // any copy take it. Where the send window is not used, the mark
            // is the process's, as before it.
            let leader_marked = (!self.models_signal_targets
                || !self.sigchld_window_usable(parent))
                && self.blocked.sigchld_ready.insert(parent);
            if let Some(deferred) = self.deferred_child_exit_sigchld(parent, tid) {
                // That thread already dequeued a `SIGCHLD` of this process (the
                // kernel's own, raised when the tracer released the child), and
                // its delivery was deferred. A standard signal coalesces, so
                // that delivery stands for this exit: commit it now instead of
                // sending a second one, which would also find the thread in no
                // pool at all.
                self.undefer_sigchld(deferred);
                self.blocked.sigchld_ready.insert(deferred);
                self.readmit_ready_thread(deferred);
                if let SignalTimerId::ChildExit { child, .. } = id {
                    self.child_exit_sigchld.stand_in(parent, child);
                }
            } else {
                // Where the scheduler models signal targets, send to the thread
                // the kernel gives this shared-queue `SIGCHLD`
                // (`fire_child_exit_alarm`): the child's creator (`tid`) if it
                // does not block the signal and no `rt_sigsuspend` sibling the
                // kernel would wake instead exists, otherwise the next thread
                // that does not. Only if that thread sleeps outside the
                // schedule is it woken (`arm_signaled_background`); a gated
                // wait holds `SIGCHLD` until its call returns
                // (https://github.com/rrnewton/hermit/issues/3146).
                let SignalTimerId::ChildExit { child, .. } = id else {
                    unreachable!("matched a ChildExit timer");
                };
                let receiver = match self.send_child_exit_sigchld(parent, tid, child) {
                    super::child_exit_sigchld::ChildExitSend::NotOwned => self
                        .sending_controlled_child_exit(child, |s| {
                            s.fire_child_exit_alarm(parent, tid, None, None)
                        }),
                    super::child_exit_sigchld::ChildExitSend::Nothing => {
                        // No copy will take the process's mark set above.
                        // Without publication, nothing would clear it before a
                        // later, host-timed SIGCHLD delivery to the leader took
                        // it and skipped its deferral.
                        if leader_marked && self.child_exit_publication().is_err() {
                            self.blocked.sigchld_ready.remove(&parent);
                        }
                        None
                    }
                    super::child_exit_sigchld::ChildExitSend::Sent(receiver) => receiver,
                };
                // A receiver armed for the release barrier carries its own
                // mark (`arm_signaled_background`) and takes this signal at a
                // fixed point, so the process's mark set above would outlive
                // it and could later let another, host-timed `SIGCHLD`
                // delivery to the leader skip its deferral (the Codex review
                // of https://github.com/rrnewton/hermit/commit/2f046e41b49d4b24b1bec605f269e3c3afde5895).
                if leader_marked
                    && receiver.is_some_and(|receiver| {
                        receiver != parent && self.blocked.signaled_background.contains(&receiver)
                    })
                {
                    self.blocked.sigchld_ready.remove(&parent);
                }
            }
        } else if !self.divert_in_guest_fatal_alarm(deadline, id, tid, signal, observed_now) {
            if matches!(id, SignalTimerId::Posix(..)) {
                self.fire_posix_timer(deadline, id, target, signal);
            } else if let SignalTimerId::Alarm(process) = id {
                self.fire_itimer_real(deadline, process, tid, signal);
            } else {
                // Without publication reports, a timer dispatched here (an empty
                // run queue's wake) takes the child-exit ledger's decision too:
                // it records the scheduler's send, or sends nothing where the
                // kernel's copy was delivered first.
                let nothing_to_send = self.child_exit_publication().is_err()
                    && match id {
                        SignalTimerId::ChildExit { child, parent } => {
                            self.child_exit_sigchld.timer(parent, child)
                                == Some(super::child_exit_sigchld::TimerSend::Nothing)
                        }
                        _ => false,
                    };
                if !nothing_to_send {
                    match id {
                        // A controlled child exit's send is its publication's
                        // effect (`child_exit_sink`).
                        SignalTimerId::ChildExit { child, .. } => self
                            .sending_controlled_child_exit(child, |s| {
                                s.fire_alarm(id.process(), tid, signal)
                            }),
                        _ => self.fire_alarm(id.process(), tid, signal),
                    }
                }
            }
        }
    }

    /// What Linux does with an expiration of a timer of `process` that
    /// notifies with real-time signal `signo` (`target`: armed by this thread,
    /// or notifying exactly this one), as far as the guest could tell
    /// (https://github.com/rrnewton/hermit/issues/4089). The threads below are
    /// the process's live threads, or for a thread-directed timer only its
    /// named thread, if live: Linux queues that expiration on the thread's own
    /// queue and drops it once the thread is gone
    /// (https://github.com/rrnewton/hermit/issues/4164). A thread-directed
    /// expiration kept pending is recorded for the whole process, so a call of
    /// any thread that could observe it refuses: more refusals than Linux
    /// would justify, never a silent difference.
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
    /// Each mask is the one the schedule fixes (`thread_signal_mask_for`, or the
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
        target: timed_waiters::SignalTarget,
        signo: i32,
    ) -> RealTimeFate {
        let bit = kernel_signal_bit(signo);
        if !self.models_signal_targets {
            return match self.read_sleeper_signal_state(target.tid()) {
                Ok(state) if state.discarded_at_generation() & bit != 0 => RealTimeFate::Discarded,
                Err(SleeperStateUnreadable::Gone) => RealTimeFate::Discarded,
                _ => RealTimeFate::Deliverable,
            };
        }
        let mut threads = self.process_signal_targets(process);
        if let timed_waiters::SignalTarget::Thread(named) = target {
            threads.retain(|thread| *thread == named);
        }
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
                None => match self.thread_signal_mask_for(thread, signo) {
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
        // Its description may be shared with any process.
        self.note_replay_take_cover(None, mask);
        // The call may have changed the mask of a descriptor a background read
        // already holds (`signalfd4` on an existing descriptor, whose
        // description any process may share), so a read out now may read
        // SIGCHLD whatever its descriptor read when it began
        // (`outside_call_may_take_sigchld`).
        if mask & super::kernel_signal_bit(libc::SIGCHLD) != 0 {
            for read in self.background_reads.values_mut() {
                if matches!(read.signals, super::BackgroundReadSignals::Signalfd(_)) {
                    read.signals = super::BackgroundReadSignals::Unknown;
                }
            }
        }
        // Its reads are not observed by the fallback's records
        // (`thread_timer_copies`), and its description may be shared with any
        // process (`signalfd_may_read`), so every process's records of a
        // covered signal go.
        self.thread_timer_copies
            .retain(|(_, signo), _| mask & super::kernel_signal_bit(*signo) == 0);
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
    pub(super) fn deferred_child_exit_sigchld(
        &self,
        process: DetPid,
        creator: DetTid,
    ) -> Option<DetTid> {
        if !self.models_signal_targets {
            // A delivery the child-exit ledger matched to another child's exit
            // stands for that one (`ChildExitSigchldLedger::attributed`),
            // where publication is not reported (with it, as before).
            return (self.blocked.sigchld_deferred.contains(&process)
                && (self.child_exit_publication().is_ok()
                    || !self.child_exit_sigchld.attributed.contains(&process)))
            .then_some(process);
        }
        // A delivery the child-exit ledger matched to another child's exit
        // stands for that one, not this (`ChildExitSigchldLedger::attributed`).
        let stands_in = |thread: &DetTid| {
            self.sigchld_process(*thread) == process
                && !self.child_exit_sigchld.attributed.contains(thread)
        };
        if self.blocked.sigchld_deferred.contains(&creator) && stands_in(&creator) {
            return Some(creator);
        }
        self.blocked
            .sigchld_deferred
            .iter()
            .copied()
            .find(|thread| stands_in(thread))
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
        let siginfo = super::discarded_at_send::itimer_real_siginfo();
        let event = SignalEvent::new(
            libc::SIGALRM,
            siginfo,
            SignalTarget::Process { pid: process.tgid },
        )
        .map_err(|_| ProtocolFailure::Identity)?;
        // Linux sends the expiry to the leader (`it_real_fn` names
        // `leader_pid`), and `prepare_signal` drops it there when the leader
        // leaves SIGALRM unblocked and ignores it
        // (<https://github.com/rrnewton/hermit/issues/4929>): nothing is
        // queued, so nothing is published, and the timer waits for a
        // shared-queue SIGALRM dequeue, as the traced model's does
        // (`leader_ignores_sigalrm`).
        let leader = DetTid::from_raw(pid.as_raw());
        if self.discarded_when_sent_to(leader, libc::SIGALRM, None) {
            tracing::info!(
                "[dpid {}] SIGALRM expired while its leader ignores it; discarded at generation, the timer waits for a dequeue.",
                pid
            );
            self.real_timers.discard_expiry(pid, deadline)?;
            return Ok(());
        }
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
        self.parked
            .note_timer_publication(pid, process, event, deadline, &outcome);
        self.real_timers.publication(expiry, outcome)?;
        // A new copy in the shared queue: Linux's `complete_signal` chooses the
        // thread it wakes for it now, and moves the process's `curr_target`
        // when its walk chose that thread. A copy the queue already held
        // chooses nothing (`legacy_queue`).
        if let ProcessSignalPublicationResult::Committed(receipt) = outcome
            && !receipt.coalesced
        {
            self.note_alarm_generation(pid)?;
        }
        Ok(())
    }

    /// Publishes one expiration of `process`'s process-directed POSIX timer
    /// `timer` through the installed control, as Linux queues a
    /// `SIGEV_SIGNAL` timer's signal on the process's shared queue with
    /// `SI_TIMER` and the timer's id. The expiration was popped at its
    /// virtual-time deadline (`dispatch_timed_signal`), and the publication
    /// commits under the same scheduler lock, so what is pending after it is a
    /// function of the schedule. A waiter it should interrupt is found the
    /// way an `ITIMER_REAL` expiration's is, by the recipients the backend
    /// reports for the pending signal (`select_parked_alarm`), and a new
    /// SIGALRM copy goes through the same generation step
    /// (`note_alarm_generation`): Linux runs `complete_signal` for it too
    /// (`posixtimer_queue_sigqueue`, kernel/signal.c:1943-1953 at v6.17). A
    /// copy Linux discards when it is sent, one the leader leaves unblocked
    /// and ignores, is not published at all, and chooses no thread.
    ///
    /// `si_overrun` is 0, as `timer_getoverrun` reports; the timer's
    /// `sigev_value` is not retained by `timer_create`, so `si_value` is 0.
    // TODO-HUMAN-REVIEW(PR-1019): Review routing POSIX timer expirations
    // through the backend's ProcessSignalControl::publish_timer_expiry.
    fn publish_posix_timer_expiry(
        &mut self,
        pid: DetPid,
        timer: i32,
        signal: nix::sys::signal::Signal,
    ) -> Result<(), ProtocolFailure> {
        let process = self.real_timers.process_identity(pid)?;
        let control = self
            .parked
            .control
            .clone()
            .ok_or(ProtocolFailure::Identity)?;
        let signo = signal as libc::c_int;
        // Linux's `send_sigqueue` checks the signal against the leader
        // (`pid_task` of the timer's process; no thread of it runs where the
        // scheduler publishes the expiry), and `prepare_signal` drops it there
        // when the leader leaves it unblocked and ignores it (`sig_ignored`,
        // kernel/signal.c:104-123 and 1966-2030 at v6.12): nothing is queued,
        // `complete_signal` chooses no thread and `curr_target` stays where it
        // was. So nothing is published, as for an `ITIMER_REAL` expiry
        // (`publish_real_expiry`), and no SIGALRM receiver is chosen for it
        // (the Claude review of <https://github.com/rrnewton/hermit/issues/4929>,
        // slice 2, round 4). The timer itself rearms as before. Named at DEBUG,
        // not INFO: the traced model records no such line, and a KVM-only INFO
        // record would lower the parity credit of every cell where it appears
        // (`note_alarm_generation`).
        let leader = DetTid::from_raw(pid.as_raw());
        if self.discarded_when_sent_to(leader, signo, None) {
            tracing::debug!(
                "[dpid {}] POSIX timer {} expired with signal {} while its leader ignores it; discarded at generation.",
                pid,
                timer,
                signo
            );
            return Ok(());
        }
        let mut siginfo = [0; 128];
        siginfo[..4].copy_from_slice(&signo.to_ne_bytes());
        siginfo[8..12].copy_from_slice(&libc::SI_TIMER.to_ne_bytes());
        // `si_timerid`, the first field of the kernel's `_timer` member.
        siginfo[16..20].copy_from_slice(&timer.to_ne_bytes());
        let event = SignalEvent::new(signo, siginfo, SignalTarget::Process { pid: process.tgid })
            .map_err(|_| ProtocolFailure::Identity)?;
        let outcome = control.process.publish_timer_expiry(process, event);
        // Due at the expiration being dispatched (`signal_sent_at`).
        let due = self.signal_sent_at();
        self.parked
            .note_timer_publication(pid, process, event, due, &outcome);
        match outcome {
            ProcessSignalPublicationResult::Committed(receipt) => {
                // A new SIGALRM copy: the walk chooses its thread and moves
                // `curr_target`, D1 is named, and a fatal copy D1 leaves with
                // no receiver is refused (<https://github.com/rrnewton/hermit/issues/4929>).
                // A copy the queue already held chooses nothing, as Linux's
                // timer adds an overrun instead (TRACE_SIGNAL_ALREADY_PENDING,
                // kernel/signal.c:2061-2064).
                if signo == libc::SIGALRM && !receipt.coalesced {
                    self.note_alarm_generation(pid)?;
                }
                Ok(())
            }
            ProcessSignalPublicationResult::FailedAfterCommit { .. } => {
                self.parked.failures.push(process);
                Ok(())
            }
            outcome @ ProcessSignalPublicationResult::RejectedBeforeCommit(_) => {
                Err(TimerFailure::Publication(outcome).into())
            }
        }
    }

    /// Publishes a parent-death signal that Detcore decided where a thread
    /// left (`Scheduler::deliver_parent_death_signals`), through the installed
    /// control, at the drain that sends it (`flush_parent_death_signals`). It
    /// goes to the whole child process as Linux's `forget_original_parent`
    /// sends it, with `group_send_sig_info(SEND_SIG_NOINFO, PIDTYPE_TGID)`:
    /// `SI_USER`, `si_pid` the thread group of the thread that left, and
    /// `si_uid` that thread's real user ID, which is 0 for every guest task.
    /// The publication commits under the scheduler lock at a fixed point of
    /// the schedule, and a parked waiter it should interrupt is found as a
    /// timer expiration's is, by the recipients the backend reports for the
    /// pending signal (`select_parked_alarm`). A refused publication ends the
    /// run as a backend failure; one that failed after committing is
    /// forwarded (`flush_signal_failures`).
    pub(super) fn publish_parent_death_signal(&mut self, pending: super::PendingParentDeathSignal) {
        if let Err(failure) = self.try_publish_parent_death_signal(pending) {
            tracing::error!(
                "[scheduler] publishing parent-death signal {} to process {} failed: {:?}",
                pending.signal,
                pending.child,
                failure
            );
            if let Some(wake) =
                self.report_backend_failure_location(super::BackendFailureLocation {
                    pid: reverie::Pid::from_raw(pending.child.as_raw()),
                    tid: None,
                    phase: "KVM parent-death signal publication",
                })
            {
                self.parked.failure_wakes.push(wake);
                self.parked.wake.try_put(());
            }
        }
    }

    fn try_publish_parent_death_signal(
        &mut self,
        pending: super::PendingParentDeathSignal,
    ) -> Result<(), ProtocolFailure> {
        let process = self.real_timers.process_identity(pending.child)?;
        let control = self
            .parked
            .control
            .clone()
            .ok_or(ProtocolFailure::Identity)?;
        let signo = pending.signal as libc::c_int;
        let mut siginfo = [0; 128];
        siginfo[..4].copy_from_slice(&signo.to_ne_bytes());
        siginfo[8..12].copy_from_slice(&libc::SI_USER.to_ne_bytes());
        // `si_pid` and `si_uid`, the fields of the kernel's `_kill` member.
        siginfo[16..20].copy_from_slice(&pending.sender.as_raw().to_ne_bytes());
        siginfo[20..24].copy_from_slice(&0_u32.to_ne_bytes());
        let event = SignalEvent::new(signo, siginfo, SignalTarget::Process { pid: process.tgid })
            .map_err(|_| ProtocolFailure::Identity)?;
        match control.process.publish_parent_death_signal(process, event) {
            ProcessSignalPublicationResult::Committed(_) => {}
            ProcessSignalPublicationResult::FailedAfterCommit { .. } => {
                self.parked.failures.push(process);
            }
            ProcessSignalPublicationResult::RejectedBeforeCommit(_) => {
                return Err(ProtocolFailure::Identity);
            }
        }
        *self
            .parked
            .published_parent_death_signals
            .entry(pending.child)
            .or_default() |= kernel_signal_bit(signo);
        Ok(())
    }

    /// Called by the daemon only after the quiescent due-event drain. Backend
    /// membership is intersected with ordered scheduler admission and actual
    /// operation capabilities; host callback arrival is never a tie breaker.
    ///
    /// For each process with a SIGALRM pending in its shared queue that some
    /// thread can take (the backend's `alarm_recipients`), one thread is
    /// chosen, by Linux's `complete_signal` rule (`alarm_receiver`), and only
    /// it is acted on, by how it can take the copy (`alarm_reach`,
    /// <https://github.com/rrnewton/hermit/issues/4929>):
    /// - parked in a wait the observation protocol can interrupt, it is
    ///   observed, as before;
    /// - queued to run, with a filed ordinary request, step 3 resumes it
    ///   before any other thread (`alarm_takers`), so it is the thread that
    ///   takes the copy at its return to user mode, as the thread Linux wakes
    ///   for it does;
    /// - polling a call whose probe never takes a pending signal (`poll`,
    ///   `epoll_wait`, a receive, an `rt_sigtimedwait`), it is woken
    ///   (`wake_polling_alarm_receiver`) and resumed first, so its call
    ///   returns for the signal and it takes the copy at that return, as
    ///   Linux's `signal_wake_up` makes the call return;
    /// - polling a call whose probe takes the signal itself under the call's
    ///   own temporary mask (`select`, `pselect6`), or a vfork parent, which
    ///   Linux cannot wake either, nothing is done for it, and no other
    ///   thread is chosen in its place: Linux wakes the one thread, and this
    ///   one takes the copy at its own next probe or return to user mode.
    ///
    /// A thread this backend cannot give the copy to (a futex wait, a call
    /// outside the schedule) is never chosen (`alarm_wants`, the named KVM
    /// gap D1).
    ///
    /// Before any of this, a thread parked in a wait the observation can
    /// interrupt that has a signal of its own pending, which it neither
    /// blocks nor ignores, is observed, whether or not a SIGALRM is pending
    /// (`observe_parked_thread_woken_by_its_own_signal`): Linux woke it when
    /// that signal was sent, so it is in no wait the receiver walk could pass
    /// over or choose.
    ///
    /// Before this choice, the first eligible parked recipient by thread ID was
    /// observed even where Linux gives the copy to a runnable leader that
    /// wants it, which interrupted a parked sibling's wait that Linux leaves
    /// asleep; and a runnable recipient was never scheduled for the copy, so
    /// a thread that blocks SIGALRM could run on with the copy pending and
    /// the timer stopped, for as long as an uncapped slice lasts.
    ///
    /// The takers are derived anew at every call: the last call before a
    /// selection, the empty-queue step's and the selection's own, sees the
    /// state step 3 then selects from. A taker holds its priority only until
    /// its return to user mode takes the copy: that empties the shared queue,
    /// so the next call chooses nothing, and the turn after it is the run
    /// queue's own choice. Between any two grants by alarm priority, step 3
    /// makes at least one choice of the run queue's own
    /// (`alarm_priority_available`): two processes whose recurring timers fall
    /// due again within each other's grants, or one process whose POSIX timer
    /// rearms whether or not its copy was taken, could otherwise keep their
    /// takers first for ever, and no other runnable thread would run. And the
    /// priority never serves a delivery that is not a sigreturn continuation,
    /// nor a `sched_yield` (`takes_alarm_priority`).
    ///
    /// Where the SIGALRM choice began no observation of the process, the
    /// parent-death signals Detcore published to it
    /// (`published_parent_death_signals`) are asked about next, in signal
    /// order: the backend reports the threads that could take each one now,
    /// and the first eligible parked one, in that order, is woken to take it.
    pub(super) fn select_parked_alarm(&mut self) -> Result<(), SelectionFailure> {
        let Some(control) = self.parked.control.clone() else {
            return Ok(());
        };
        self.parked.alarm_takers.clear();
        // A woken poller leaves `alarm_woken` when step 3 resumes it; one
        // that left the run queue another way (it was killed) leaves here.
        let run_queue = &self.run_queue;
        self.parked
            .alarm_woken
            .retain(|thread| run_queue.contains_tid(*thread));
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
            // One observation of a process at a time. An observation selects
            // from the process's shared queue on the observed thread's own
            // host thread, so a second one begun before the first selected
            // could race it for the same pending signal. The scheduler cannot
            // reach this pass while an observed thread's request is still
            // empty (`are_all_quiesced`); this keeps the rule local to the
            // selection as well. An ordinary boundary permit (`site` None)
            // does not count: its holder selected inside its own serial grant,
            // before it filed the request that let this pass run, and Linux
            // wakes a parked thread for a process signal while a sibling is
            // still on its way to deliver its own.
            if self
                .parked
                .permits
                .values()
                .any(|p| p.task.process == process && p.site.is_some())
            {
                continue;
            }
            if self.observe_parked_thread_woken_by_its_own_signal(pid)? {
                continue;
            }
            if self.select_alarm_receiver(&control, pid, process)? {
                continue;
            }
            let published = self
                .parked
                .published_parent_death_signals
                .get(&pid)
                .copied()
                .unwrap_or(0);
            let signals = (1..=31).filter(|signal| {
                *signal != libc::SIGALRM && published & kernel_signal_bit(*signal) != 0
            });
            'signals: for signal in signals {
                let recipients = control
                    .process
                    .signal_recipients(process, signal)
                    .map_err(|_| process_failure(ProtocolFailure::Identity))?;
                for recipient in recipients {
                    let tid = DetTid::from_raw(recipient.task.tid.as_raw());
                    // A child wait that a ready child or ECHILD completes is
                    // admitted so that ordinary completion can win.
                    self.admit_completed_child_wait(pid, tid, recipient.task)?;
                    if self.parked_alarm_observable(pid, tid, recipient.task)? {
                        // Other wait families remain pending for their
                        // existing return boundary; this does not claim that
                        // their interruption works.
                        self.begin_alarm_observation(pid, tid, recipient.task)
                            .map_err(|failure| SelectionFailure {
                                pid,
                                tid: Some(tid),
                                failure,
                            })?;
                        break 'signals;
                    }
                }
            }
        }
        Ok(())
    }

    /// The SIGALRM step of `select_parked_alarm` for `pid`, whose exact
    /// identity is `process`: the thread Linux's `complete_signal` chooses
    /// (`alarm_receiver`) is acted on by how it can take the copy
    /// (`alarm_reach`). Returns whether it began an observation of the
    /// process, which then admits no other one in this pass.
    fn select_alarm_receiver(
        &mut self,
        control: &reverie::BackendSignalControl,
        pid: DetPid,
        process: reverie::SignalProcessId,
    ) -> Result<bool, SelectionFailure> {
        let recipients =
            control
                .process
                .alarm_recipients(process)
                .map_err(|_| SelectionFailure {
                    pid,
                    tid: None,
                    failure: ProtocolFailure::Identity,
                })?;
        if recipients.is_empty() {
            return Ok(false);
        }
        // A child wait that a ready child or ECHILD completes is admitted
        // so that ordinary completion can win, whichever thread the signal
        // goes to.
        for recipient in &recipients {
            let tid = DetTid::from_raw(recipient.task.tid.as_raw());
            self.admit_completed_child_wait(pid, tid, recipient.task)?;
        }
        let Some(tid) = self.alarm_receiver(pid, &recipients) else {
            return Ok(false);
        };
        let Some((_, task)) = self.real_timers.task_identity(pid, tid) else {
            return Ok(false);
        };
        match self.alarm_reach(pid, tid, task) {
            AlarmReach::Parked => {
                let Some(recipient) = recipients
                    .iter()
                    .find(|r| DetTid::from_raw(r.task.tid.as_raw()) == tid)
                else {
                    return Ok(false);
                };
                if self.parked_alarm_observable(pid, tid, recipient.task)? {
                    // Other wait families remain pending for their
                    // existing return boundary; this does not claim that
                    // their interruption works.
                    self.begin_alarm_observation(pid, tid, recipient.task)
                        .map_err(|failure| SelectionFailure {
                            pid,
                            tid: Some(tid),
                            failure,
                        })?;
                    return Ok(true);
                }
            }
            AlarmReach::Queued => {
                if self.next_turns.get(&tid).is_some_and(|turn| {
                    turn.protocol.owner == NextTurnOwner::Ordinary
                        && !turn.req.is_empty()
                        && !turn.resp.is_full()
                }) {
                    self.parked.alarm_takers.insert(pid, tid);
                }
            }
            AlarmReach::Poll => {
                // Linux wakes the thread it chose (`signal_wake_up`,
                // kernel/signal.c:1032 at v6.17), and its call returns for
                // the signal. This one's probe never takes a pending
                // signal (`poll`, `epoll_wait`, a receive), so it is woken
                // here, and resumed first. Not while the alarm priority is
                // spent: woken then, it would wait behind the run queue's
                // own choice, which could take the copy first.
                if self.alarm_priority_available() && self.wake_polling_alarm_receiver(tid) {
                    self.parked.alarm_takers.insert(pid, tid);
                }
            }
            // A backend-served `rt_sigsuspend` is woken for the copy by the
            // step-2 drain after this generation (`wake_backend_signal_waits`).
            AlarmReach::TakesAtItsProbe
            | AlarmReach::BackendSuspend
            | AlarmReach::VforkParent
            | AlarmReach::OutOfReach => {}
        }
        Ok(false)
    }

    /// Observe the first thread of `pid`, in thread-ID order, that is parked
    /// in a wait the observation protocol can interrupt
    /// (`parked_alarm_observable`: a `pause`, a `nanosleep` whose deadline is
    /// ahead, a polled read, a child wait) and has a signal of its own
    /// pending that it neither blocks nor ignores. Returns whether it began an
    /// observation, which then admits no other one of the process in this
    /// pass.
    ///
    /// Linux never leaves such a thread asleep. A thread-directed send names
    /// its target, and `complete_signal` wakes it (`wants_signal`, then
    /// `signal_wake_up`, kernel/signal.c:946-961 and 1033 at v6.17,
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v6.17#n1033>),
    /// which ends its interruptible wait; a thread that reaches `pause` with a
    /// signal already pending returns at once (`sys_pause`, lines 4822-4829).
    /// The woken thread dequeues its private queue before the shared one
    /// (`dequeue_signal`, lines 618-640), so it takes that signal first, as
    /// the observation delivers it. This backend does not wake a parked wait
    /// for a thread-directed signal of its own accord, so without this the
    /// thread slept until a pending SIGALRM's observation happened to reach
    /// it, or for ever (<https://github.com/rrnewton/hermit/issues/4929>);
    /// and it must not steer the SIGALRM receiver walk, which would pass it
    /// over as busy and give every copy to a sibling Linux does not choose.
    ///
    /// Read at the quiesced point where the receiver is chosen, from the
    /// scheduler's committed state and the one signal-state reader, as the
    /// walk reads it (`alarm_wants`): a thread whose exit or exec is in flight
    /// is not read (`reads_settled_signal_state`), and a temporary mask comes
    /// from committed state (`controlled_temporary_mask`). An ignored pending
    /// copy, which Linux would have dropped as it was sent, does not count.
    fn observe_parked_thread_woken_by_its_own_signal(
        &mut self,
        pid: DetPid,
    ) -> Result<bool, SelectionFailure> {
        for tid in self.process_signal_targets(pid) {
            let Some((_, task)) = self.real_timers.task_identity(pid, tid) else {
                continue;
            };
            if !self.parked_alarm_observable(pid, tid, task)? || !self.has_own_signal_to_take(tid) {
                continue;
            }
            self.begin_alarm_observation(pid, tid, task)
                .map_err(|failure| SelectionFailure {
                    pid,
                    tid: Some(tid),
                    failure,
                })?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Whether `thread` has a signal pending for it alone that it neither
    /// blocks nor ignores (Linux's `task_sigpending` after `prepare_signal`
    /// dropped the ignored ones), read as `alarm_wants` reads a busy thread.
    fn has_own_signal_to_take(&self, thread: DetTid) -> bool {
        if !self.reads_settled_signal_state(thread) {
            return false;
        }
        let Ok(temporary) = self.controlled_temporary_mask(thread) else {
            return false;
        };
        self.read_guest_signal_state(thread).is_ok_and(|state| {
            let mask = temporary.unwrap_or(state.blocked);
            state.thread_pending & !mask & !state.ignored_at_generation() != 0
        })
    }

    /// The thread Linux's `complete_signal` wakes for a SIGALRM in `pid`'s
    /// shared queue (`process_signal_receiver::complete_signal`, the rule the
    /// traced model uses for a guest's process-directed send): the leader,
    /// which the expiry names (`it_real_fn` sends to `leader_pid`; a POSIX
    /// timer's `posixtimer_get_target` names the leader too, unless a thread
    /// of the process is running, and none runs where the scheduler publishes
    /// the expiry, between turns; kernel/signal.c:1966-1974 at v6.17), if it
    /// wants the signal, and otherwise the first thread that does, walking the
    /// thread list from the process's `curr_target`
    /// (<https://github.com/rrnewton/hermit/issues/4929>). `None` when no
    /// thread wants it: the copy then waits for the first thread that can
    /// take it, as in Linux.
    ///
    /// A thread wants the signal (`alarm_wants`) when its exit and its
    /// process's exec are not in flight, this backend can give it the copy
    /// (`alarm_reach`), its mask leaves SIGALRM unblocked (the backend's
    /// `recipients`, or a temporary mask, which the backend's set does not
    /// hold), and no other signal it leaves unblocked is pending for it alone
    /// (Linux's `task_sigpending`, read through the one signal-state reader).
    /// Read at a quiesced point, so every input is the schedule's.
    pub(super) fn alarm_receiver(
        &self,
        pid: DetPid,
        recipients: &[reverie::SignalRecipient],
    ) -> Option<DetTid> {
        self.alarm_receiver_and_walk(pid, recipients, AlarmReachRule::Backend)
            .0
    }

    /// [`Self::alarm_receiver`] under `rule`, with the thread the walk chose,
    /// which becomes the process's `curr_target` at the generation.
    pub(super) fn alarm_receiver_and_walk(
        &self,
        pid: DetPid,
        recipients: &[reverie::SignalRecipient],
        rule: AlarmReachRule,
    ) -> (Option<DetTid>, Option<DetTid>) {
        let leader = DetTid::from_raw(pid.as_raw());
        let threads = self.alarm_wants(pid, recipients, rule);
        let curr_target = self
            .signal_curr_targets
            .get(&pid)
            .copied()
            .unwrap_or(leader);
        match super::process_signal_receiver::complete_signal(&threads, leader, curr_target) {
            (
                super::process_signal_receiver::ProcessSignalReceiver::Thread { thread, .. },
                moved,
            ) => (Some(thread), moved),
            _ => (None, None),
        }
    }

    /// Whether each live thread of `pid`, in thread-ID order, wants a SIGALRM
    /// now (`alarm_receiver`), as Linux's `wants_signal` asks it
    /// (kernel/signal.c:946-961 at v6.17): not exiting, not blocking SIGALRM,
    /// and no other unblocked signal pending for it alone.
    ///
    /// The thread's state is decided first, from the scheduler's committed
    /// state (`alarm_reach`), and only then its mask, so a request filed at a
    /// moment host timing chooses is never read: the mask Linux's `blocked`
    /// holds now is a temporary mask where one is installed
    /// (`controlled_temporary_mask`, which the backend's set does not hold),
    /// and otherwise the thread's own, as the backend's `recipients` reflect
    /// it; either without the set an `rt_sigtimedwait` waits for
    /// (`sigtimedwait_sets`), which Linux unblocks for the wait
    /// (kernel/signal.c:3776-3777). An unknown temporary mask makes the
    /// choice unknown, and nothing is chosen.
    ///
    /// Under `AlarmReachRule::Backend`, on a backend that cannot interrupt a
    /// blocked wait for a signal (`passes_over_waits_it_cannot_interrupt`),
    /// one more thread is passed over, which Linux would choose: one waiting
    /// where Linux interrupts the wait for the signal (`signal_wake_up`) but
    /// this backend cannot (`AlarmReach::OutOfReach`: a futex wait, a call
    /// outside the schedule, or another wait outside the run queue that
    /// neither the observation protocol nor the step-2 drain ends; a
    /// backend-served `rt_sigsuspend`, which the drain ends, is counted,
    /// `AlarmReach::BackendSuspend`). It cannot take the copy until
    /// that wait ends, so choosing it would leave the copy pending, and could
    /// leave the run without progress where the signal's handler is what ends
    /// the wait; passing it over, as Linux passes over a thread that cannot
    /// take a signal now (a stopped or traced one, lines 957-958), lets the
    /// walk reach a thread that can. This is the named KVM gap D1, until the
    /// backend can interrupt such a wait (N8a; tracked under
    /// <https://github.com/rrnewton/hermit/issues/4485>). The
    /// thread is decided first, before its request slot is read
    /// (`reads_settled_signal_state`). `AlarmReachRule::Linux` counts it, to
    /// name the difference and to refuse a fatal copy that would otherwise
    /// wait (`note_alarm_generation`). A vfork parent is not passed over:
    /// Linux cannot wake it either (`wait_for_vfork_done` sleeps killable),
    /// so the copy waits for it there too.
    fn alarm_wants(
        &self,
        pid: DetPid,
        recipients: &[reverie::SignalRecipient],
        rule: AlarmReachRule,
    ) -> Vec<(DetTid, super::process_signal_receiver::Wants)> {
        use super::process_signal_receiver::Wants;
        let bit = kernel_signal_bit(libc::SIGALRM);
        self.process_signal_targets(pid)
            .into_iter()
            .map(|thread| {
                let Some((_, identity)) = self.real_timers.task_identity(pid, thread) else {
                    return (thread, Wants::No);
                };
                // D1, keyed on the backend's capability: the pass-over ends when
                // the backend can interrupt such a wait (N8a).
                if rule == AlarmReachRule::Backend
                    && self.passes_over_waits_it_cannot_interrupt()
                    && self.alarm_reach(pid, thread, identity) == AlarmReach::OutOfReach
                {
                    return (thread, Wants::No);
                }
                if !self.reads_settled_signal_state(thread) {
                    return (thread, Wants::No);
                }
                let Ok(temporary) = self.controlled_temporary_mask(thread) else {
                    return (thread, Wants::Unknown);
                };
                let recipient = recipients.iter().any(|r| r.task == identity);
                let waited = self.sigtimedwait_sets.get(&thread).copied().unwrap_or(0);
                let unblocked = waited & bit != 0
                    || match temporary {
                        Some(mask) => mask & bit == 0,
                        None => recipient,
                    };
                if !unblocked {
                    return (thread, Wants::No);
                }
                // A signal it ignores does not count: the control's pending
                // sets may hold one that Linux would have dropped as it was
                // sent (`signal_state_reader`). A thread running a call outside
                // the schedule is never busy: the backend can post a signal to
                // it alone when host timing completes that call (SIGPIPE for a
                // write that fails with EPIPE), so its pending set is not a
                // function of the schedule, and it is not read. While D1 passes
                // such a thread over, only the Linux rule reaches it, so this
                // decides only what DEBUG names and whether a fatal copy is
                // refused.
                // A vfork parent is read: no host timing ends its call or
                // posts it a signal (`Scheduler::kernel_sent_wants`).
                let outside = (self.blocked.external_io_blockers.contains_key(&thread)
                    || self.blocked.rt_sigsuspend_blockers.contains_key(&thread))
                    && !self.vfork_barriers.contains_key(&thread);
                let busy = !outside
                    && self.read_guest_signal_state(thread).is_ok_and(|state| {
                        let mask = temporary.unwrap_or(state.blocked) & !waited;
                        state.thread_pending & !mask & !state.ignored_at_generation() != 0
                    });
                (thread, if busy { Wants::Busy } else { Wants::Yes })
            })
            .collect()
    }

    /// Whether the receiver choice passes over a thread waiting where this
    /// backend cannot interrupt the wait for a signal (D1,
    /// `AlarmReach::OutOfReach`): exactly while the backend lacks
    /// `supports_blocked_wait_signal_interruption`. N8a gives KVM that
    /// capability, and the choice then counts such a thread, as Linux does;
    /// N8a must also interrupt the wait then, or the copy waits for it
    /// (<https://github.com/rrnewton/hermit/issues/4929>).
    pub(super) fn passes_over_waits_it_cannot_interrupt(&self) -> bool {
        !self.backend.supports_blocked_wait_signal_interruption
    }

    /// How `tid`, a live thread of `pid` whose exact lifetime is `task`, can
    /// take a SIGALRM pending in its process's shared queue if it is the
    /// thread Linux's `complete_signal` chooses (`AlarmReach`).
    ///
    /// Decided from the scheduler's committed state: a vfork barrier, the
    /// pools of calls running outside the schedule, the parked request's
    /// origin, a backend-served `rt_sigsuspend`'s filed request, and run-queue
    /// membership. A thread outside the schedule files
    /// its rejoin when host timing finishes its call, so its request is not
    /// read. A queued thread's request is read, for which call it polls: it
    /// filed that request in its own turn.
    fn alarm_reach(
        &self,
        pid: DetPid,
        tid: DetTid,
        task: reverie::SignalTaskIdentity,
    ) -> AlarmReach {
        if self.vfork_barriers.contains_key(&tid) {
            return AlarmReach::VforkParent;
        }
        if self.blocked.external_io_blockers.contains_key(&tid)
            || self.blocked.rt_sigsuspend_blockers.contains_key(&tid)
        {
            return AlarmReach::OutOfReach;
        }
        let Some(turn) = self.next_turns.get(&tid) else {
            return AlarmReach::OutOfReach;
        };
        if let Some(origin) = self.parked_alarm_origin(pid, tid, task)
            && matches!(
                origin.control,
                ControlCapability::ParkedWait { .. }
                    | ControlCapability::PolledRead { .. }
                    | ControlCapability::ChildWait { .. }
            )
            && !turn.req.is_empty()
            && !turn.resp.is_full()
        {
            return AlarmReach::Parked;
        }
        // S2b's park of an `rt_sigsuspend` the backend serves: an indefinite
        // sleep, held in the timed waiters, that the step-2 drain ends
        // (`wake_backend_signal_waits`). Linux sleeps interruptibly there under
        // the call's mask, so a signal that mask lets in ends the wait, and a
        // fatal one ends the process (`sigsuspend`, kernel/signal.c:4835-4846
        // at v7.1.3; `complete_signal`'s fatal path, lines 999-1027). Passed
        // over as out of reach, a default-action SIGALRM was refused (exit
        // 122) where Linux, ptrace and the base ended the process (the coord7
        // review of https://github.com/rrnewton/hermit/pull/5221, finding 1).
        if self.backend_suspend_mask(tid).is_some() {
            return AlarmReach::BackendSuspend;
        }
        if !self.run_queue.contains_tid(tid) {
            return AlarmReach::OutOfReach;
        }
        let Some(Ok(request)) = turn.req.try_read() else {
            return AlarmReach::Queued;
        };
        if request.resources.len() != 1
            || !request
                .resources
                .contains_key(&ResourceID::InternalIOPolling)
            || self.waitid_signal_request(tid).is_some()
        {
            return AlarmReach::Queued;
        }
        // A `poll` and an unmasked `ppoll` carry the EINTR marker too, as the
        // errno a handler gives them, for the idle-poller diagnosis
        // (`Scheduler::poller_may_run_external_handler`), but their probe
        // never ends for a pending signal, so they are woken below. The
        // request names its call (`Resources::fyi`). A `ppoll` under a
        // temporary mask polls here on KVM since SIGSHARE S2b
        // (`Detcore::backend_masked_ppoll_wait`): each probe carries the
        // call's mask, so the backend applies it inside the call and the probe
        // takes a pending signal that mask lets in, as `pselect6`'s does, and
        // its request files that mask (`blocked_signal_mask`). It is told from
        // an unmasked `ppoll` by that mask. Passed over below as out of reach,
        // a lone masked `ppoll` that Linux chooses would have had a fatal
        // SIGALRM refused (the coord7 review of
        // https://github.com/rrnewton/hermit/pull/5221, finding 5).
        if request.signal_interrupt_errno() == Some(reverie::syscalls::Errno::EINTR.into_raw())
            && (matches!(request.fyi.as_str(), "select" | "pselect6")
                || (request.fyi.as_str() == "ppoll" && request.blocked_signal_mask.is_some()))
        {
            return AlarmReach::TakesAtItsProbe;
        }
        if request.blocked_signal_mask.is_some() || request.blocked_signal_mask_unknown {
            // No polled call carries a temporary mask without the EINTR
            // marker today. Waking one would drop the mask with its request,
            // so it is passed over rather than given a copy its probe may
            // never take.
            return AlarmReach::OutOfReach;
        }
        AlarmReach::Poll
    }

    /// Wake `tid`, the receiver of a SIGALRM pending in its process's shared
    /// queue, which polls a call whose probe never ends for a pending signal
    /// (`AlarmReach::Poll`): replace its filed request with the signal and
    /// queue it at the front of its band, as the scheduler wakes a polling
    /// thread for a signal it sends (`wake_signaled_guest`). Its turn answers
    /// `Signaled`; the call makes its attempt and, if it would still block,
    /// ends with its interrupt errno: EINTR for `epoll_wait`; for a `poll` or
    /// an unmasked `ppoll`, EINTR where the first signal its return to user
    /// mode delivers has a handler, and ERESTARTSYS otherwise, decided again
    /// where that return is committed (`woken_poll_errno`,
    /// `settle_woken_wait_at_return`), the deliveries Linux would settle
    /// otherwise refused by name (`check_woken_wait_delivery`); for a
    /// receive, ERESTARTSYS, or EINTR
    /// where its socket has a finite receive timeout
    /// (`interrupt_errno_at_entry`); for a blocking send on a recorded socket,
    /// ERESTARTSYS where its return delivers only ignored copies and it sent
    /// nothing yet, and a refusal by name otherwise (`network_send_wait`). The
    /// backend restarts a call that ends with ERESTARTSYS where no handler
    /// runs, and the thread takes the copy at that return to user mode.
    /// Returns whether it woke the thread.
    fn wake_polling_alarm_receiver(&mut self, tid: DetTid) -> bool {
        let ordinary = self.next_turns.get(&tid).is_some_and(|turn| {
            turn.protocol.owner == NextTurnOwner::Ordinary
                && !turn.req.is_empty()
                && !turn.resp.is_full()
        });
        if !ordinary
            || !self.run_queue.contains_tid(tid)
            || self
                .happens_before
                .as_ref()
                .is_some_and(|hb| hb.parked.contains(&tid))
        {
            return false;
        }
        let removed = self.run_queue.remove_tid(tid);
        debug_assert!(removed, "a queued poller left the run queue");
        let mut request = Resources::new(tid);
        request.insert(
            ResourceID::InboundSignal(crate::types::SigWrapper(libc::SIGALRM)),
            Permission::W,
        );
        let woken = self.force_unblock_thread_at(tid, request, true);
        if woken {
            self.parked.alarm_woken.insert(tid);
        }
        woken
    }

    /// At a new copy of `pid`'s SIGALRM in its shared queue, published by the
    /// scheduler: Linux's `complete_signal` runs once per generated copy and
    /// moves the process's `curr_target` to the thread its walk chose
    /// (`alarm_receiver`, <https://github.com/rrnewton/hermit/issues/4929>).
    ///
    /// Where the thread Linux wakes waits where this backend cannot interrupt
    /// it (D1, `alarm_wants`), the difference is named at DEBUG, for every
    /// such copy. Not at INFO: the INFO stream is what `--verify-strict`
    /// compares between runs and what the parity post-pass compares between
    /// backends, and this record exists on KVM alone, so at INFO it would
    /// lower the parity credit of every cell where it appears, as a KVM-only
    /// request field did (`Scheduler::logged_polling_request`). It never
    /// reaches the guest.
    ///
    /// Where that copy's action is the default, which ends the process, and
    /// no thread the backend can give it to wants it, the run is refused by
    /// name (`fatal_alarm_refusal`), so a passed-over thread never delays a
    /// fatal signal.
    pub(super) fn note_alarm_generation(&mut self, pid: DetPid) -> Result<(), ProtocolFailure> {
        let Some(control) = self.parked.control.clone() else {
            return Ok(());
        };
        let process = self.real_timers.process_identity(pid)?;
        let recipients = control
            .process
            .alarm_recipients(process)
            .map_err(|_| ProtocolFailure::Identity)?;
        let (chosen, moved) =
            self.alarm_receiver_and_walk(pid, &recipients, AlarmReachRule::Backend);
        let linux = if chosen.is_none() || tracing::enabled!(tracing::Level::DEBUG) {
            self.alarm_receiver_and_walk(pid, &recipients, AlarmReachRule::Linux)
                .0
        } else {
            None
        };
        if let Some(linux) = linux
            && Some(linux) != chosen
        {
            tracing::debug!(
                "[dtid {}] SIGALRM for process {} goes to {}: Linux wakes this thread, \
                 which waits where this backend cannot interrupt the wait (a futex \
                 wait, a call outside the schedule, or another wait outside the run \
                 queue; KVM gap D1 until N8a, \
                 https://github.com/rrnewton/hermit/issues/4929)",
                linux,
                pid,
                chosen.map_or_else(|| "no thread yet".to_string(), |t| format!("dtid {t}")),
            );
            if chosen.is_none()
                && let Some(refusal) = self.fatal_alarm_refusal(pid, linux)
            {
                super::exit_on_scheduler_refusal(&refusal);
            }
        }
        if let Some(moved) = moved {
            self.signal_curr_targets.insert(pid, moved);
        }
        Ok(())
    }

    /// The named refusal for a new SIGALRM copy in `pid`'s shared queue whose
    /// action is the default, which ends the process, when Linux gives it to
    /// `linux` but no thread this backend can give it to wants it now (D1,
    /// `note_alarm_generation`).
    ///
    /// For a fatal signal, Linux's `complete_signal` ends the whole thread
    /// group at once, whatever each thread waits in (`sig_fatal`: the group
    /// exit and a SIGKILL to every thread, kernel/signal.c:999-1027 at v6.17).
    /// This backend ends a process for a signal only through a thread that
    /// takes it, and `linux` waits where the backend cannot interrupt the wait,
    /// while every other thread blocks SIGALRM, so the copy would wait for
    /// that wait to end, or forever: an `alarm()` watchdog over a
    /// `pthread_join` would fire only once the join returned. The run is
    /// refused rather than let the passed-over thread delay the process's end.
    /// `None` for a copy a handler catches or one that is ignored, which the
    /// D1 pass-over leaves to the next thread that can take it, and for a
    /// state that cannot be read.
    pub(super) fn fatal_alarm_refusal(&self, pid: DetPid, linux: DetTid) -> Option<String> {
        let state = self.read_guest_signal_state(linux).ok()?;
        state.default_action_ends_process(libc::SIGALRM).then(|| {
            format!(
                "hermit: unsupported on the KVM backend: interrupting a futex wait, a call \
                 outside the schedule, or another wait outside the run queue for a signal. \
                 A SIGALRM for process {pid}, whose action is the default, which ends the \
                 process, goes to dtid {linux}, which waits in such a wait, and no other \
                 thread can take it now; Linux ends the process at once, and here it would end \
                 only when that wait does, or never; refusing the run rather than delaying it \
                 (KVM gap D1 until N8a, https://github.com/rrnewton/hermit/issues/4929)"
            )
        })
    }

    /// [`Self::first_alarm_taker_entry`]'s thread, for the tests.
    #[cfg(test)]
    pub(super) fn first_alarm_taker(&self) -> Option<DetTid> {
        self.first_alarm_taker_entry().map(|(_, thread)| thread)
    }

    /// The thread step 3 resumes before any other because it is to take a
    /// SIGALRM pending in its process's shared queue (`alarm_takers`,
    /// `select_parked_alarm`), with its process, if any: the first in process
    /// order still queued to run and served by the priority
    /// (`takes_alarm_priority`), while the priority is available
    /// (`alarm_priority_available`).
    pub(super) fn first_alarm_taker_entry(&self) -> Option<(DetPid, DetTid)> {
        if !self.alarm_priority_available() {
            return None;
        }
        self.parked
            .alarm_takers
            .iter()
            .map(|(pid, thread)| (*pid, *thread))
            .find(|(_, thread)| {
                self.run_queue.contains_non_polling_tid(*thread)
                    && self.takes_alarm_priority(*thread)
            })
    }

    /// Whether step 3 may resume an alarm taker by alarm priority now: not
    /// after a grant by that priority with no choice of the run queue's own
    /// since (`alarm_priority_served`), whichever process it served. Linux
    /// wakes a receiver for its signal; it never runs it ahead of every other
    /// runnable thread again and again. Two processes' takers, or one
    /// process's taker whose POSIX timer rearms whether or not its copy was
    /// taken, otherwise held the priority for ever while a runnable thread
    /// never ran (the Codex reviews of 4929 slice 2, rounds 1 and 2, P1).
    pub(super) fn alarm_priority_available(&self) -> bool {
        self.parked.alarm_priority_served.is_none()
    }

    /// Whether the alarm priority serves `thread`, a queued alarm taker, given
    /// the request it filed in its own turn. Not a signal delivery that does
    /// not continue its thread's own `rt_sigreturn`
    /// (`Resources::continues_sigreturn` unset): such a delivery waits its place
    /// behind the other runnable threads by design, after a stop that ended
    /// its slice, Linux's TIF_NEED_RESCHED case, or after
    /// `MAX_SIGRETURN_CONTINUATIONS` deliveries in a row, and the priority must
    /// not put it first again, which reset that count and let one process's
    /// handler run for ever. The delivery a poller woken for the copy filed
    /// for it (`alarm_woken`) is served. And not a `sched_yield`, by which
    /// the thread gave its turn to the other runnable threads.
    ///
    /// A thread whose slice ran out is served: a worker that computes between
    /// calls ends its turns only so, and taking its copy late kept a stopped
    /// `ITIMER_REAL` stopped through the next thread's slice (`mixedsample`
    /// 3 of 20 positive instead of 19).
    pub(super) fn takes_alarm_priority(&self, thread: DetTid) -> bool {
        if self.parked.alarm_woken.contains(&thread) {
            return true;
        }
        let Some(Some(Ok(request))) = self.next_turns.get(&thread).map(|turn| turn.req.try_read())
        else {
            return true;
        };
        let unmarked_delivery =
            Self::is_inbound_signal_turn(&request) && !request.continues_sigreturn;
        !unmarked_delivery && !request.resources.contains_key(&ResourceID::SchedYield)
    }

    /// Admit `tid`'s blocked child wait, if it is one that a ready child or
    /// ECHILD completes: a selector change can leave a blocked waiter without
    /// a matching child, and its original request must then be admitted so
    /// that ordinary completion can actually win. Selection and readiness are
    /// tested under the same scheduler lock, before reserving or dequeuing a
    /// signal.
    fn admit_completed_child_wait(
        &mut self,
        pid: DetPid,
        tid: DetTid,
        task: reverie::SignalTaskIdentity,
    ) -> Result<(), SelectionFailure> {
        let Some(origin) = self.parked_alarm_origin(pid, tid, task) else {
            return Ok(());
        };
        if !matches!(origin.control, ControlCapability::ChildWait { .. }) {
            return Ok(());
        }
        let Some(Ok(resources)) = self
            .next_turns
            .get(&tid)
            .and_then(|turn| turn.req.try_read())
        else {
            return Ok(());
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
        if (self.ready_child_wait(parent, spec).is_some()
            || !self.has_child_wait_target(parent, spec))
            && self.blocked.child_waiters.remove(&tid).is_some()
        {
            self.runqueue_push_back(tid);
        }
        Ok(())
    }

    /// The origin of `tid`'s filed request, where `tid` is a live thread of
    /// `pid` whose exact lifetime is `task` and whose request the observation
    /// protocol could own: its incarnation current, not logically killed,
    /// holding no permit, and its turn ordinarily owned.
    fn parked_alarm_origin(
        &self,
        pid: DetPid,
        tid: DetTid,
        task: reverie::SignalTaskIdentity,
    ) -> Option<ResourceOrigin> {
        let (mm, identity) = self.real_timers.task_identity(pid, tid)?;
        if identity != task
            || !self.rpc_incarnation_matches(tid, mm)
            || self.thread_is_logically_killed(tid)
            || self.parked.permits.contains_key(&tid)
        {
            return None;
        }
        let turn = self.next_turns.get(&tid)?;
        if turn.protocol.owner != NextTurnOwner::Ordinary {
            return None;
        }
        turn.protocol.origin
    }

    /// Whether `tid`, a live thread of `pid` whose exact lifetime is `task`,
    /// is parked in a wait the observation protocol can interrupt for a
    /// pending signal: a `pause`, a `nanosleep` whose deadline is still ahead,
    /// a polled read, or a child wait that no ready child or ECHILD completes;
    /// its request filed and not yet answered.
    fn parked_alarm_observable(
        &self,
        pid: DetPid,
        tid: DetTid,
        task: reverie::SignalTaskIdentity,
    ) -> Result<bool, SelectionFailure> {
        let Some(origin) = self.parked_alarm_origin(pid, tid, task) else {
            return Ok(false);
        };
        let Some(turn) = self.next_turns.get(&tid) else {
            return Ok(false);
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
                    return Ok(false);
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
                if self.ready_child_wait(parent, spec).is_some()
                    || !self.has_child_wait_target(parent, spec)
                {
                    return Ok(false);
                }
                LogicalTime::INDEFINITE
            }
            _ => return Ok(false),
        };
        Ok(deadline > self.committed_time && !turn.req.is_empty() && !turn.resp.is_full())
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
        if turn.resp.is_full() {
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
        // The parked wait leaves the scheduler to observe the signal: other
        // threads may have run since it parked, as at a grant
        // (`Scheduler::granted`).
        self.granted.insert(tid);
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
        // Taken whatever the outcome: only a caught interruption by exactly
        // this removal uses it.
        let observed = self.parked.observed_timer_dequeues.remove(&wait.dettid);
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
                || turn.req.is_full()
                || turn.resp.is_full()
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
                ObservationFinish::InterruptForCaught { selection, dequeue } => {
                    if selection.site != site {
                        return Err(ProtocolFailure::Identity);
                    }
                    // A `nanosleep` interrupted by the caught signal its own
                    // observation removed, a timer's publication: Linux ends
                    // the sleep at the timer's due time, so the sleep's
                    // remaining time is taken from there, however late the
                    // finish (https://github.com/rrnewton/hermit/issues/4931).
                    if let ControlCapability::ParkedWait {
                        policy: ParkedWaitPolicy::NanosleepNoHandlerRestart { absolute_deadline },
                        ..
                    } = owned.origin.control
                        && let Some(observed) = observed
                        && observed.wait == wait
                        && observed.lease == lease
                        && dequeue == Some(observed.dequeue)
                        && observed.due < absolute_deadline
                    {
                        self.blocked
                            .sleep_interruptions
                            .insert(wait.dettid, observed.due);
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
                || response.is_full()
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
        if self.parked.wake.is_full() {
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
            || turn.resp.is_full()
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

    /// The scheduler is about to replace `tid`'s filed request with one it
    /// substitutes: a signal wake (`force_unblock_thread_at`), a signal batch
    /// merged into such a wake, a pause answered by a due SIGALRM, or a
    /// change point's empty request. The substitute ends the logical
    /// operation the old request stood for: the thread's next grant answers
    /// it, the parked call returns that answer (`capable_resource_request`
    /// returns on any grant), and a restarted or retried call files a new
    /// request with new ownership. Nothing reads the old ownership after the
    /// replacement:
    /// - an observation of the substitute is refused by the capability's
    ///   resource checks (`begin_alarm_observation`);
    /// - finish and resume intents need an Observing or
    ///   AwaitingResumeRegistration phase, which hold a permit or an empty
    ///   request, never the request being replaced;
    /// - only exec (`complete_signal_exec`) and the grant read it, and the
    ///   grant settles only ownership of the request it answers.
    ///
    /// So retire the exact waiting ownership of the old request, as
    /// `retire_child_wait_signal_request` does for a child wait, rather than
    /// rebinding it to a transport that no longer describes it, and drop the
    /// parked capability, which cannot describe the substitute either.
    /// Without this the grant left the ownership behind, and only the
    /// thread's next signal-boundary receipt removed it. reverie-kvm skips
    /// an empty boundary and sends no receipt there, so a later exec was
    /// refused.
    ///
    /// Ownership of the replaced request must be a plain waiting one: owner
    /// Ordinary, no permit, the response not yet answered, and the entry's
    /// phase, response and origin those of the turn. Anything else is a
    /// protocol failure, never silently retired. A request with no parked
    /// capability (every request on a backend without process signal
    /// control) is left untouched.
    pub(crate) fn retire_replaced_parked_request(
        &mut self,
        tid: DetTid,
    ) -> Result<(), ProtocolFailure> {
        let Some(turn) = self.next_turns.get(&tid) else {
            return Ok(());
        };
        let Some(origin) = turn.protocol.origin else {
            return Ok(());
        };
        if !matches!(
            origin.control,
            ControlCapability::ParkedWait { .. }
                | ControlCapability::PolledRead { .. }
                | ControlCapability::ChildWait { .. }
        ) {
            return Ok(());
        }
        let mut owned = self
            .parked
            .requests
            .values()
            .filter(|owned| owned.request == turn.req)
            .peekable();
        let retires = owned.peek().is_some();
        if retires
            && (turn.protocol.owner != NextTurnOwner::Ordinary
                || self.parked.permits.contains_key(&tid)
                || turn.resp.is_full())
        {
            return Err(ProtocolFailure::Phase);
        }
        for owned in owned {
            if !matches!(owned.phase, ContinuationPhase::Waiting)
                || owned.response != turn.resp
                || owned.origin != origin
            {
                return Err(ProtocolFailure::Identity);
            }
        }
        self.settle_parked_grant(tid);
        if retires {
            // At DEBUG, as the INFO stream is compared between runs and
            // backends, and only this backend's control owns parked
            // requests. KVM refuses an exec by any thread but the leader,
            // and a thread's exit retires what it owns, so an exec can
            // witness a stale ownership only on the leader; a test reads this
            // record to witness it on any thread
            // (`run_kvm_a_signal_ending_a_resumed_parked_read_retires_its_ownership`).
            tracing::debug!(
                "[dtid {}] retired the waiting parked ownership of the request a substitute replaced",
                tid
            );
        }
        let turn = self
            .next_turns
            .get_mut(&tid)
            .expect("checked request owner");
        turn.protocol
            .origin
            .as_mut()
            .expect("checked request origin")
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
                && turn.resp.is_empty())
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

/// How a test corrupts the waiting ownership of a thread's current request,
/// to reach each refusal of `retire_replaced_parked_request`.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum OwnedRequestCorruption {
    /// The entry is no longer in the Waiting phase.
    NotWaiting,
    /// The entry names another response transport than the turn's.
    ForeignResponse,
    /// The entry names another origin than the turn's.
    ForeignOrigin,
}

#[cfg(test)]
impl Scheduler {
    /// Corrupts the ownership of `tid`'s current request as `how` says.
    pub(crate) fn corrupt_owned_request_for_test(
        &mut self,
        tid: DetTid,
        how: OwnedRequestCorruption,
    ) {
        let request = self.next_turns[&tid].req.clone();
        let owned = self
            .parked
            .requests
            .values_mut()
            .find(|owned| owned.request == request)
            .expect("ownership of the current request");
        match how {
            OwnedRequestCorruption::NotWaiting => {
                owned.phase =
                    ContinuationPhase::Observing(ParkedObservationLease { nonce: u64::MAX });
            }
            OwnedRequestCorruption::ForeignResponse => owned.response = Ivar::new(),
            OwnedRequestCorruption::ForeignOrigin => {
                owned.origin.mm = MmId::initial(DetTid::from_raw(999_999));
            }
        }
    }

    /// Gives `tid`'s current request a waiting parked ownership again, with a
    /// PolledRead capability at `site`, so that the next substitution of that
    /// request (a merge in place, say) has live ownership to retire.
    pub(crate) fn own_current_request_for_test(&mut self, tid: DetTid, site: CallbackSignalSite) {
        let nonce = self.parked.nonce + 1;
        self.parked.nonce = nonce;
        let turn = self.next_turns.get_mut(&tid).expect("registered thread");
        let origin = turn.protocol.origin.as_mut().expect("filed request origin");
        origin.control = ControlCapability::PolledRead { site };
        let origin = *origin;
        let resources = turn
            .req
            .try_read()
            .expect("filed request")
            .expect("ordinary request");
        let id = ContinuationId { dettid: tid, nonce };
        let owned = OwnedRequest {
            id,
            request: turn.req.clone(),
            response: turn.resp.clone(),
            resources,
            origin,
            ready_read: None,
            membership: None,
            cycle: 0,
            phase: ContinuationPhase::Waiting,
        };
        self.parked.requests.insert(id, owned);
    }
}
