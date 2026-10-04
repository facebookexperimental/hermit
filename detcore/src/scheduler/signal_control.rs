/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Run-scoped publication and consuming return-boundary ownership.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

use reverie::BackendSignalControl;
use reverie::BackendSignalControlMode;
use reverie::ChildExitPublication;
use reverie::ChildExitPublicationResult;
use reverie::ExitStatus;
use reverie::SignalBoundaryOutcome;
use reverie::SignalBoundaryReceipt;
use reverie::SignalDeliveryPermit;
use reverie::SignalProcessId;
use reverie::SignalTaskIdentity;

use super::Scheduler;
use super::parked::ChildExitReservation;
use super::parked::ChildExitReservationPhase;
use super::parked::CompletedChildExit;
use super::parked::ExitBoundaryFence;
use super::parked::FinalProcessClass;
use super::parked::NextTurnOwner;
use super::parked::ProcessGeneration;
use super::parked::ProtocolFailure;
use super::parked::TerminalProcess;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::MmId;

/// Whether an exit continues through the Tool-controlled KVM boundary.
///
/// This is intentionally typed: callers must not infer control from an
/// optional permit or from the backend kind and accidentally emit the legacy
/// timed `SIGCHLD` in addition to the generation-bound publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExitReserveMode {
    Uncontrolled,
    Controlled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildExitBegin {
    Publish,
    ExactDuplicate,
}

struct ChildExitFinish {
    forward: Option<(BackendSignalControl, ChildExitPublication)>,
    rejected: Option<reverie::syscalls::Errno>,
    protocol_failure: Option<ProtocolFailure>,
    wakes: Vec<futures::channel::oneshot::Sender<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildExitPollState {
    Admission,
    Completion,
    Done,
}

/// A deliberately two-poll child-publication callback.
///
/// Poll one performs the complete synchronous scheduler admission and backend
/// publication while holding the scheduler mutex.  It then drops the lock,
/// self-wakes, and returns `Pending`, which is the exact fence KVM polls before
/// exposing waitability.  Poll two consumes the retained typed result and only
/// then removes the stage-2 barrier.  `Drop` fails closed and releases that
/// barrier if run cancellation abandons the retained callback between polls.
pub(crate) struct ChildExitPublicationFuture {
    sched: Arc<Mutex<Scheduler>>,
    event: reverie::BackendChildWaitEvent,
    state: ChildExitPollState,
}

impl ChildExitPublicationFuture {
    pub(crate) fn new(sched: Arc<Mutex<Scheduler>>, event: reverie::BackendChildWaitEvent) -> Self {
        Self {
            sched,
            event,
            state: ChildExitPollState::Admission,
        }
    }

    fn protocol_error(failure: ProtocolFailure) -> reverie::Error {
        let errno = match failure {
            ProtocolFailure::Overflow => reverie::syscalls::Errno::EOVERFLOW,
            ProtocolFailure::Unsupported | ProtocolFailure::UnexpectedControl => {
                reverie::syscalls::Errno::ENOSYS
            }
            ProtocolFailure::Identity
            | ProtocolFailure::Phase
            | ProtocolFailure::Timer(_)
            | ProtocolFailure::Observation(_) => reverie::syscalls::Errno::EINVAL,
        };
        errno.into()
    }

    fn forward_failure(
        forward: Option<(BackendSignalControl, ChildExitPublication)>,
    ) -> Result<(), reverie::Error> {
        if let Some((control, receipt)) = forward {
            control
                .process
                .finish_child_exit_publication_failure(receipt)?;
        }
        Ok(())
    }

    fn complete_outside_lock(finish: ChildExitFinish) -> Result<(), reverie::Error> {
        let forwarded = Self::forward_failure(finish.forward);
        for wake in finish.wakes {
            let _ = wake.send(());
        }
        if let Some(errno) = finish.rejected {
            return Err(errno.into());
        }
        forwarded?;
        if let Some(failure) = finish.protocol_failure {
            return Err(Self::protocol_error(failure));
        }
        Ok(())
    }
}

impl Future for ChildExitPublicationFuture {
    type Output = Result<(), reverie::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.state {
            ChildExitPollState::Admission => {
                let event = self.event;
                let (result, wakes) = {
                    let mut sched = self.sched.lock().unwrap();
                    let result = sched.begin_child_exit_publication(event);
                    let wakes = result.as_ref().err().map_or_else(Vec::new, |_| {
                        sched.mark_child_exit_publication_terminal(
                            event,
                            "KVM child-exit publication admission",
                        )
                    });
                    (result, wakes)
                };
                match result {
                    Ok(ChildExitBegin::Publish | ChildExitBegin::ExactDuplicate) => {
                        self.state = ChildExitPollState::Completion;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                    Err(failure) => {
                        self.state = ChildExitPollState::Done;
                        for wake in wakes {
                            let _ = wake.send(());
                        }
                        Poll::Ready(Err(Self::protocol_error(failure)))
                    }
                }
            }
            ChildExitPollState::Completion => {
                let event = self.event;
                let (finish, error_wakes) = {
                    let mut sched = self.sched.lock().unwrap();
                    let finish = sched.finish_child_exit_publication(event);
                    let wakes = finish.as_ref().err().map_or_else(Vec::new, |_| {
                        sched.mark_child_exit_publication_terminal(
                            event,
                            "KVM child-exit publication completion",
                        )
                    });
                    (finish, wakes)
                };
                self.state = ChildExitPollState::Done;
                let finish = match finish {
                    Ok(finish) => finish,
                    Err(failure) => {
                        for wake in error_wakes {
                            let _ = wake.send(());
                        }
                        return Poll::Ready(Err(Self::protocol_error(failure)));
                    }
                };
                Poll::Ready(Self::complete_outside_lock(finish))
            }
            ChildExitPollState::Done => {
                panic!("child-exit publication future polled after completion")
            }
        }
    }
}

impl Drop for ChildExitPublicationFuture {
    fn drop(&mut self) {
        if self.state == ChildExitPollState::Done {
            return;
        }
        let (finish, failure_wake) = {
            let mut sched = self.sched.lock().unwrap();
            let failure_wake = if sched.backend_failed() {
                None
            } else {
                sched.report_backend_failure_location(super::BackendFailureLocation {
                    pid: self.event.child.tgid,
                    tid: None,
                    phase: "KVM child-exit publication callback cancelled",
                })
            };
            let finish = sched.cancel_child_exit_publication(self.event).ok();
            (finish, failure_wake)
        };
        if let Some(finish) = finish {
            let _ = Self::complete_outside_lock(finish);
        }
        if let Some(wake) = failure_wake {
            let _ = wake.send(());
        }
    }
}

impl Scheduler {
    pub(crate) fn take_signal_failure_wakes(
        &mut self,
    ) -> Vec<futures::channel::oneshot::Sender<()>> {
        std::mem::take(&mut self.parked.failure_wakes)
    }

    pub(crate) fn install_signal_control(
        &mut self,
        control: Option<BackendSignalControl>,
    ) -> Result<BackendSignalControlMode, reverie::Error> {
        if !self.kvm_shared_dequeue_timers {
            return Ok(BackendSignalControlMode::Unchanged);
        }
        if self.parked.control.is_some() || !self.next_turns.is_empty() {
            return Err(reverie::syscalls::Errno::EINVAL.into());
        }
        self.parked.control = Some(control.ok_or(reverie::syscalls::Errno::ENOSYS)?);
        Ok(BackendSignalControlMode::ToolControlled)
    }

    pub(crate) fn authorize_signal_boundary(
        &mut self,
        task: SignalTaskIdentity,
    ) -> Result<Option<SignalDeliveryPermit>, reverie::Error> {
        let Some(control) = self.parked.control.clone() else {
            return Ok(None);
        };
        let tid = DetTid::from_raw(task.tid.as_raw());
        if self.backend_failed() || self.thread_is_logically_killed(tid) {
            return Ok(None);
        }
        if let Some(permit) = self.parked.permits.get(&tid) {
            return if permit.task == task {
                Ok(Some(*permit))
            } else {
                Err(reverie::syscalls::Errno::EINVAL.into())
            };
        }
        // Real serial grant ownership, not arrival at this backend hook, is
        // the authority. Startup is registered before its first grant as well.
        if self.parked.running != Some(tid) {
            return Ok(None);
        }
        let pid = self
            .registered_process(tid)
            .ok_or(reverie::syscalls::Errno::ESRCH)?;
        let (mm, expected) = self
            .real_timers
            .task_identity(pid, tid)
            .ok_or(reverie::syscalls::Errno::ESRCH)?;
        if expected != task || !self.rpc_incarnation_matches(tid, mm) {
            return Err(reverie::syscalls::Errno::EINVAL.into());
        }
        if self
            .parked
            .permits
            .values()
            .any(|p| p.task.process == task.process)
        {
            return Ok(None);
        }
        let sequence = self
            .parked
            .nonce
            .checked_add(1)
            .ok_or(reverie::syscalls::Errno::EOVERFLOW)?;
        let permit = SignalDeliveryPermit {
            task,
            sequence,
            site: None,
        };
        control.process.reserve_delivery(permit)?;
        self.parked.nonce = sequence;
        self.parked.permits.insert(tid, permit);
        Ok(Some(permit))
    }

    fn final_exit_after_boundary(&mut self, tid: DetTid, process: DetPid, group: bool) -> bool {
        if group {
            return true;
        }
        self.thread_tree
            .my_thread_group(&process)
            .into_iter()
            .filter(|member| self.next_turns.contains_key(member))
            .all(|member| member == tid)
    }

    fn terminal_identity_for_pid(
        &self,
        process: DetPid,
    ) -> Result<SignalProcessId, ProtocolFailure> {
        let mut matches = self
            .parked
            .terminal_processes
            .values()
            .filter_map(|terminal| {
                (terminal.process.tgid.as_raw() == process.as_raw()).then_some(terminal.process)
            });
        let identity = matches.next().ok_or(ProtocolFailure::Identity)?;
        if matches.next().is_some() {
            // A numeric PID was reused and the direct family edge did not retain
            // its generation. Never guess which terminal lifetime owns it.
            return Err(ProtocolFailure::Identity);
        }
        Ok(identity)
    }

    fn classify_final_process(
        &self,
        process: DetPid,
    ) -> Result<FinalProcessClass, ProtocolFailure> {
        let Some(parent) = self.thread_tree.parent_process(&process) else {
            return Ok(FinalProcessClass::Root);
        };
        match self.real_timers.process_identity(parent) {
            Ok(parent) => Ok(FinalProcessClass::LiveParent { parent }),
            Err(_) => Ok(FinalProcessClass::DirectParentTerminal {
                parent: self.terminal_identity_for_pid(parent)?,
            }),
        }
    }

    fn prepare_child_exit_reservation(
        &mut self,
        tid: DetTid,
        process: DetPid,
        group: bool,
        child: SignalProcessId,
    ) -> Result<Option<ChildExitReservation>, ProtocolFailure> {
        if child.tgid.as_raw() != process.as_raw()
            || !self.final_exit_after_boundary(tid, process, group)
        {
            return if child.tgid.as_raw() == process.as_raw() {
                Ok(None)
            } else {
                Err(ProtocolFailure::Identity)
            };
        }
        match self.classify_final_process(process)? {
            FinalProcessClass::LiveParent { parent } => Ok(Some(ChildExitReservation {
                child,
                parent,
                phase: ChildExitReservationPhase::AwaitingTerminalStatus,
            })),
            FinalProcessClass::Root | FinalProcessClass::DirectParentTerminal { .. } => Ok(None),
        }
    }

    /// Record the exact final process transition before stage 1 is removed.
    /// Install or advance stage 2 in the same scheduler critical section, so
    /// the control barrier never opens between terminal membership and physical
    /// cleanup. A live parent's child-publication transaction owns its stage 2;
    /// roots and children of terminal parents need a separate retirement receipt.
    fn record_final_process_transition(
        &mut self,
        tid: DetTid,
        process: DetPid,
        group: bool,
        child: SignalProcessId,
        status: ExitStatus,
    ) -> Result<Option<FinalProcessClass>, ProtocolFailure> {
        if !self.final_exit_after_boundary(tid, process, group) {
            return Ok(None);
        }
        if child.tgid.as_raw() != process.as_raw() {
            return Err(ProtocolFailure::Identity);
        }
        let key = ProcessGeneration::from_backend(child);
        let class = match self.parked.child_exit_reservations.get_mut(&key) {
            Some(reservation) => {
                if reservation.child != child
                    || !matches!(
                        reservation.phase,
                        ChildExitReservationPhase::AwaitingTerminalStatus
                    )
                {
                    return Err(ProtocolFailure::Phase);
                }
                reservation.phase = ChildExitReservationPhase::AwaitingPublication { status };
                FinalProcessClass::LiveParent {
                    parent: reservation.parent,
                }
            }
            None => {
                let class = self.classify_final_process(process)?;
                if let FinalProcessClass::LiveParent { parent } = class
                    && self
                        .parked
                        .child_exit_reservations
                        .insert(
                            key,
                            ChildExitReservation {
                                child,
                                parent,
                                phase: ChildExitReservationPhase::AwaitingPublication { status },
                            },
                        )
                        .is_some()
                {
                    return Err(ProtocolFailure::Phase);
                }
                class
            }
        };
        let terminal = TerminalProcess {
            process: child,
            class,
            status,
        };
        match self.parked.terminal_processes.insert(key, terminal) {
            None => {}
            Some(existing) if existing == terminal => {}
            Some(_) => return Err(ProtocolFailure::Identity),
        }
        if matches!(
            class,
            FinalProcessClass::Root | FinalProcessClass::DirectParentTerminal { .. }
        ) {
            let retirement = reverie::BackendProcessRetirement {
                process: child,
                status,
            };
            if self.parked.completed_process_retirements.contains_key(&key) {
                return Err(ProtocolFailure::Phase);
            }
            match self.parked.process_retirements.insert(key, retirement) {
                None => {}
                Some(existing) if existing == retirement => {}
                Some(_) => return Err(ProtocolFailure::Identity),
            }
        }
        Ok(Some(class))
    }

    /// Consume physical cleanup without granting a turn or publishing SIGCHLD.
    /// Validate before removing any barrier; a malformed receipt makes the run
    /// terminal while the scheduler mutex still excludes ordinary grants.
    pub(crate) fn consume_process_retirement(
        &mut self,
        event: reverie::BackendProcessRetirement,
    ) -> Result<(), reverie::Error> {
        if self.parked.control.is_none() || self.backend_failed() {
            return Ok(());
        }
        let result = self.finish_process_retirement(event);
        if result.is_err()
            && let Some(wake) =
                self.report_backend_failure_location(super::BackendFailureLocation {
                    pid: event.process.tgid,
                    tid: None,
                    phase: "KVM process retirement receipt",
                })
        {
            // GlobalTool sends these only after dropping the scheduler mutex.
            self.parked.failure_wakes.push(wake);
        }
        result.map_err(|_| reverie::syscalls::Errno::EINVAL.into())
    }

    fn finish_process_retirement(
        &mut self,
        event: reverie::BackendProcessRetirement,
    ) -> Result<(), ProtocolFailure> {
        let key = ProcessGeneration::from_backend(event.process);
        if let Some(completed) = self.parked.completed_process_retirements.get(&key) {
            return if *completed == event {
                Ok(())
            } else {
                Err(ProtocolFailure::Identity)
            };
        }
        let Some(terminal) = self.parked.terminal_processes.get(&key) else {
            // Synchronous faults can retire without a controlled terminal
            // boundary. Authenticate the current process generation even when
            // worker hooks already retired its final scheduler task, and do
            // not mistake an outstanding controlled transition for that case.
            if self
                .real_timers
                .retained_process_identity(key.pid)
                .map_err(|_| ProtocolFailure::Identity)?
                != event.process
            {
                return Err(ProtocolFailure::Identity);
            }
            if self
                .parked
                .permits
                .values()
                .any(|permit| permit.task.process == event.process)
                || self
                    .parked
                    .exit_fences
                    .values()
                    .any(|fence| fence.permit.task.process == event.process)
                || self.parked.child_exit_reservations.contains_key(&key)
                || self.parked.process_retirements.contains_key(&key)
            {
                return Err(ProtocolFailure::Phase);
            }
            // No fence belongs to this notification. Remember only the exact
            // duplicate/status guard, without waking selection, changing task
            // membership, or manufacturing a controlled terminal transition.
            self.parked.completed_process_retirements.insert(key, event);
            return Ok(());
        };
        if terminal.process != event.process || terminal.status != event.status {
            return Err(ProtocolFailure::Identity);
        }
        match terminal.class {
            FinalProcessClass::Root | FinalProcessClass::DirectParentTerminal { .. } => {
                if self.parked.process_retirements.get(&key) != Some(&event) {
                    return Err(ProtocolFailure::Phase);
                }
                self.parked.process_retirements.remove(&key);
            }
            FinalProcessClass::LiveParent { .. } => {
                // Its existing child-publication receipt owns the barrier.
                // Retirement neither replaces that transaction nor emits a
                // second child event, and must follow its completion.
                if self.parked.child_exit_reservations.contains_key(&key)
                    || !self
                        .parked
                        .completed_child_exits
                        .get(&key)
                        .is_some_and(|exit| {
                            exit.completion.child == event.process
                                && exit.completion.status == event.status
                                && matches!(exit.result, ChildExitPublicationResult::Committed(_))
                        })
                {
                    return Err(ProtocolFailure::Phase);
                }
            }
        }
        self.parked.completed_process_retirements.insert(key, event);
        self.wake_control_waiter();
        Ok(())
    }

    fn begin_child_exit_publication(
        &mut self,
        event: reverie::BackendChildWaitEvent,
    ) -> Result<ChildExitBegin, ProtocolFailure> {
        let completion = event
            .child_exit_completion()
            .ok_or(ProtocolFailure::Unsupported)?;
        let key = ProcessGeneration::from_backend(completion.child);
        if let Some(terminal) = self.parked.completed_child_exits.get(&key) {
            return if terminal.completion == completion {
                Ok(ChildExitBegin::ExactDuplicate)
            } else {
                Err(ProtocolFailure::Identity)
            };
        }
        let reservation = self
            .parked
            .child_exit_reservations
            .get(&key)
            .copied()
            .ok_or(ProtocolFailure::Identity)?;
        let status = match reservation.phase {
            ChildExitReservationPhase::AwaitingPublication { status } => status,
            ChildExitReservationPhase::AwaitingTerminalStatus
            | ChildExitReservationPhase::Published { .. } => {
                return Err(ProtocolFailure::Phase);
            }
        };
        if reservation.child != completion.child
            || reservation.parent != completion.parent
            || event.parent != reservation.parent
            || event.child != reservation.child
            || completion.status != status
        {
            return Err(ProtocolFailure::Identity);
        }
        let control = self
            .parked
            .control
            .clone()
            .ok_or(ProtocolFailure::Identity)?;
        // The run-scoped facade is synchronous and forbidden from calling
        // GlobalTool. Holding the scheduler mutex here is the causal fence that
        // binds publication to the deterministic exit grant.
        let result = control.process.publish_child_exit(completion);
        self.parked
            .child_exit_reservations
            .get_mut(&key)
            .expect("validated child-exit reservation")
            .phase = ChildExitReservationPhase::Published { completion, result };
        Ok(ChildExitBegin::Publish)
    }

    /// Make every callback error scheduler-terminal in the same critical
    /// section that validates or retires its stage-2 reservation. The retained
    /// senders are fired only after failure forwarding releases this mutex.
    fn mark_child_exit_publication_terminal(
        &mut self,
        event: reverie::BackendChildWaitEvent,
        phase: &'static str,
    ) -> Vec<futures::channel::oneshot::Sender<()>> {
        match self.report_backend_failure_location(super::BackendFailureLocation {
            pid: event.child.tgid,
            tid: None,
            phase,
        }) {
            Some(wake) => vec![wake],
            None => Vec::new(),
        }
    }

    fn finish_child_exit_publication(
        &mut self,
        event: reverie::BackendChildWaitEvent,
    ) -> Result<ChildExitFinish, ProtocolFailure> {
        let completion = event
            .child_exit_completion()
            .ok_or(ProtocolFailure::Unsupported)?;
        let key = ProcessGeneration::from_backend(completion.child);
        if let Some(terminal) = self.parked.completed_child_exits.get(&key).copied() {
            return if terminal.completion == completion {
                let rejected = match terminal.result {
                    ChildExitPublicationResult::RejectedBeforeCommit(errno) => Some(errno),
                    ChildExitPublicationResult::Committed(_)
                    | ChildExitPublicationResult::FailedAfterCommit { .. } => None,
                };
                let wakes = if matches!(
                    terminal.result,
                    ChildExitPublicationResult::RejectedBeforeCommit(_)
                        | ChildExitPublicationResult::FailedAfterCommit { .. }
                ) {
                    self.mark_child_exit_publication_terminal(
                        event,
                        "KVM duplicate child-exit publication failure",
                    )
                } else {
                    Vec::new()
                };
                Ok(ChildExitFinish {
                    forward: None,
                    rejected,
                    protocol_failure: None,
                    wakes,
                })
            } else {
                Err(ProtocolFailure::Identity)
            };
        }
        let reservation = self
            .parked
            .child_exit_reservations
            .get(&key)
            .copied()
            .ok_or(ProtocolFailure::Identity)?;
        let (published, result) = match reservation.phase {
            ChildExitReservationPhase::Published { completion, result } => (completion, result),
            ChildExitReservationPhase::AwaitingTerminalStatus
            | ChildExitReservationPhase::AwaitingPublication { .. } => {
                return Err(ProtocolFailure::Phase);
            }
        };
        if published != completion
            || reservation.child != completion.child
            || reservation.parent != completion.parent
        {
            return Err(ProtocolFailure::Identity);
        }

        let (receipt, rejected) = match result {
            ChildExitPublicationResult::RejectedBeforeCommit(errno) => (None, Some(errno)),
            ChildExitPublicationResult::Committed(receipt) => (Some(receipt), None),
            ChildExitPublicationResult::FailedAfterCommit { receipt, .. } => (Some(receipt), None),
        };
        let mut protocol_failure = None;
        if let Some(receipt) = receipt {
            if receipt.completion != completion {
                return Err(ProtocolFailure::Identity);
            }
            // Waitability, never the signal-publication effect, decides whether
            // scheduler shadow state survives. A waitable child remains until
            // the Tool injects the backend wait (including WNOWAIT handling);
            // an auto-reaped child has no backend status to consume.
            if !completion.waitable
                && !self.consume_child_wait(
                    DetPid::from_raw(completion.parent.tgid.as_raw()),
                    DetPid::from_raw(completion.child.tgid.as_raw()),
                )
            {
                protocol_failure = Some(ProtocolFailure::Identity);
            }
        }
        let terminal = CompletedChildExit { completion, result };
        match self.parked.completed_child_exits.insert(key, terminal) {
            None => {}
            Some(existing) if existing == terminal => {}
            Some(_) => return Err(ProtocolFailure::Identity),
        }
        let forward = match result {
            ChildExitPublicationResult::FailedAfterCommit { receipt, .. } => {
                match self.parked.control.clone() {
                    Some(control) => Some((control, receipt)),
                    None => {
                        protocol_failure = Some(ProtocolFailure::Identity);
                        None
                    }
                }
            }
            ChildExitPublicationResult::RejectedBeforeCommit(_)
            | ChildExitPublicationResult::Committed(_) => None,
        };
        let wakes = if rejected.is_some()
            || protocol_failure.is_some()
            || matches!(result, ChildExitPublicationResult::FailedAfterCommit { .. })
        {
            self.mark_child_exit_publication_terminal(event, "KVM child-exit publication failure")
        } else {
            Vec::new()
        };
        self.parked.child_exit_reservations.remove(&key);
        self.wake_control_waiter();
        Ok(ChildExitFinish {
            forward,
            rejected,
            protocol_failure,
            wakes,
        })
    }

    fn cancel_child_exit_publication(
        &mut self,
        event: reverie::BackendChildWaitEvent,
    ) -> Result<ChildExitFinish, ProtocolFailure> {
        if !self.backend_failed() {
            // Drop cancellation must become terminal before it can remove and
            // wake a stage-2 reservation. The caller establishes that state in
            // the same scheduler critical section.
            return Err(ProtocolFailure::Phase);
        }
        let completion = event
            .child_exit_completion()
            .ok_or(ProtocolFailure::Unsupported)?;
        let key = ProcessGeneration::from_backend(completion.child);
        let Some(reservation) = self.parked.child_exit_reservations.get(&key).copied() else {
            return Ok(ChildExitFinish {
                forward: None,
                rejected: None,
                protocol_failure: None,
                wakes: Vec::new(),
            });
        };
        if matches!(
            reservation.phase,
            ChildExitReservationPhase::Published { .. }
        ) {
            return self.finish_child_exit_publication(event);
        }
        self.parked.child_exit_reservations.remove(&key);
        self.wake_control_waiter();
        Ok(ChildExitFinish {
            forward: None,
            rejected: None,
            protocol_failure: None,
            wakes: Vec::new(),
        })
    }

    /// Fence one scheduler-committed exit until KVM reports the exact terminal
    /// boundary after committing its process-family snapshot. The selected
    /// `(tid, request, response)` transaction is the authority here: step 5
    /// has not yet made `parked.running` name this task.
    pub(crate) fn reserve_exit_boundary(
        &mut self,
        tid: DetTid,
        process: DetPid,
        mm: MmId,
        group: bool,
    ) -> Result<ExitReserveMode, ProtocolFailure> {
        let Some(control) = self.parked.control.clone() else {
            return Ok(ExitReserveMode::Uncontrolled);
        };
        if self.backend_failed()
            || self.registered_process(tid) != Some(process)
            || self.parked.exit_fences.contains_key(&tid)
            || self.parked.permits.contains_key(&tid)
        {
            return Err(ProtocolFailure::Identity);
        }
        let (expected_mm, task) = self
            .real_timers
            .task_identity(process, tid)
            .ok_or(ProtocolFailure::Identity)?;
        if expected_mm != mm
            || !self.rpc_incarnation_matches(tid, mm)
            || self
                .parked
                .permits
                .values()
                .any(|permit| permit.task.process == task.process)
            || self
                .parked
                .exit_fences
                .values()
                .any(|fence| fence.permit.task.process == task.process)
        {
            return Err(ProtocolFailure::Identity);
        }
        let child_exit = self.prepare_child_exit_reservation(tid, process, group, task.process)?;
        if let Some(reservation) = child_exit {
            let key = ProcessGeneration::from_backend(reservation.child);
            if self.parked.child_exit_reservations.contains_key(&key)
                || self.parked.completed_child_exits.contains_key(&key)
                || self.parked.terminal_processes.contains_key(&key)
            {
                return Err(ProtocolFailure::Phase);
            }
        }
        let sequence = self
            .parked
            .nonce
            .checked_add(1)
            .ok_or(ProtocolFailure::Overflow)?;
        let permit = SignalDeliveryPermit {
            task,
            sequence,
            site: None,
        };
        control
            .process
            .reserve_delivery(permit)
            .map_err(|_| ProtocolFailure::Identity)?;
        self.parked.nonce = sequence;
        assert!(self.parked.permits.insert(tid, permit).is_none());
        assert!(
            self.parked
                .exit_fences
                .insert(
                    tid,
                    ExitBoundaryFence {
                        permit,
                        process,
                        mm,
                        group,
                    },
                )
                .is_none()
        );
        if let Some(reservation) = child_exit {
            let key = ProcessGeneration::from_backend(reservation.child);
            assert!(
                self.parked
                    .child_exit_reservations
                    .insert(key, reservation)
                    .is_none()
            );
        }
        Ok(ExitReserveMode::Controlled)
    }

    pub(crate) fn consume_signal_boundary(
        &mut self,
        receipt: SignalBoundaryReceipt,
    ) -> Result<(), reverie::Error> {
        let tid = DetTid::from_raw(receipt.permit.task.tid.as_raw());
        if self.parked.completed.get(&tid) == Some(&receipt) {
            return Ok(());
        }
        if self.parked.permits.get(&tid) != Some(&receipt.permit) {
            return Err(reverie::syscalls::Errno::EINVAL.into());
        }
        let exit_fence = self.parked.exit_fences.get(&tid).copied();
        if let Some(fence) = exit_fence
            && (fence.permit != receipt.permit
                || fence.process.as_raw() != receipt.permit.task.process.tgid.as_raw()
                || !matches!(
                    receipt.outcome,
                    SignalBoundaryOutcome::Terminated { group, .. } if group == fence.group
                ))
        {
            return Err(reverie::syscalls::Errno::EINVAL.into());
        }
        // Fix terminal membership before the backend can join a peer blocked
        // in an RPC. The permit is the causal fence; host exit-hook arrival is
        // not a new scheduling input. Validate the complete target set before
        // changing any membership or consuming the permit.
        let mut retire = Vec::new();
        let mut final_transition = None;
        if let SignalBoundaryOutcome::Terminated { group, wait_status } = receipt.outcome {
            let pid = DetPid::from_raw(receipt.permit.task.process.tgid.as_raw());
            let current = self.real_timers.task_identity(pid, tid);
            match current {
                Some((mm, identity))
                    if identity == receipt.permit.task
                        && exit_fence
                            .is_none_or(|fence| fence.process == pid && fence.mm == mm)
                        && self.rpc_incarnation_matches(tid, mm) => {}
                None if !self.next_turns.contains_key(&tid) => {}
                _ => return Err(reverie::syscalls::Errno::EINVAL.into()),
            }
            let targets = if group {
                self.thread_tree.my_thread_group(&pid)
            } else {
                vec![tid]
            };
            for target in targets {
                if !self.next_turns.contains_key(&target) {
                    continue;
                }
                let (mm, identity) = self
                    .real_timers
                    .task_identity(pid, target)
                    .ok_or(reverie::syscalls::Errno::EINVAL)?;
                if identity.process != receipt.permit.task.process
                    || !self.rpc_incarnation_matches(target, mm)
                {
                    return Err(reverie::syscalls::Errno::EINVAL.into());
                }
                retire.push((target, pid, mm));
            }
            final_transition = self
                .record_final_process_transition(
                    tid,
                    pid,
                    group,
                    receipt.permit.task.process,
                    ExitStatus::from_raw(wait_status),
                )
                .map_err(|_| reverie::syscalls::Errno::EINVAL)?;
        }
        // Cleanup/failure may already have logically killed the task. Exact
        // duplicates remain recognizable after timer/task retirement. No turn
        // or membership is created by this consuming notification.
        self.parked.permits.remove(&tid);
        if exit_fence.is_some() {
            self.parked.exit_fences.remove(&tid);
        }
        self.parked.completed.insert(tid, receipt);
        self.parked.requests.retain(|wait, _| wait.dettid != tid);
        if let Some(turn) = self.next_turns.get_mut(&tid) {
            turn.protocol.owner = NextTurnOwner::Ordinary;
        }
        retire.sort_unstable_by_key(|(target, _, _)| *target);
        for (target, pid, mm) in retire {
            // Existing deferred queue removals, pending-RPC cancellation,
            // clear-TID wakeups and physical-hook admission stay authoritative.
            self.logically_kill_thread(&target, &pid, mm);
        }
        if let Some(FinalProcessClass::DirectParentTerminal { parent }) = final_transition {
            // The backend classifies this as run-teardown state and never emits
            // a child callback. Mirror its auto-reap only after logical exit has
            // installed the scheduler shadow status.
            if !self.consume_child_wait(
                DetPid::from_raw(parent.tgid.as_raw()),
                DetPid::from_raw(receipt.permit.task.process.tgid.as_raw()),
            ) {
                return Err(reverie::syscalls::Errno::EINVAL.into());
            }
        }
        if exit_fence.is_some() {
            self.wake_control_waiter();
        }
        Ok(())
    }
}

/// Never call the backend's RunFailure publisher with the scheduler mutex held:
/// its GlobalTool notification takes this same mutex. Mark terminal first, then
/// transfer the retained committed receipt, then notify blocked scheduler waits.
pub(crate) fn flush_signal_failures(sched: &Arc<Mutex<Scheduler>>) {
    let (control, failures, wakes) = {
        let mut s = sched.lock().unwrap();
        (
            s.parked.control.clone(),
            std::mem::take(&mut s.parked.failures),
            std::mem::take(&mut s.parked.failure_wakes),
        )
    };
    if let Some(control) = control {
        for process in failures {
            // The backend retains its typed receipt even if forwarding discovers
            // an already-closed owner. No publication or delivery is retried.
            let _ = control.process.finish_publication_failure(process);
        }
    }
    for wake in wakes {
        let _ = wake.send(());
    }
}
