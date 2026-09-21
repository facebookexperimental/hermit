/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Run-scoped publication and consuming return-boundary ownership.

use std::sync::Arc;
use std::sync::Mutex;

use reverie::BackendSignalControl;
use reverie::BackendSignalControlMode;
use reverie::SignalBoundaryOutcome;
use reverie::SignalBoundaryReceipt;
use reverie::SignalDeliveryPermit;
use reverie::SignalTaskIdentity;

use super::Scheduler;
use super::parked::NextTurnOwner;
use crate::types::DetTid;

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
        // Fix terminal membership before the backend can join a peer blocked
        // in an RPC. The permit is the causal fence; host exit-hook arrival is
        // not a new scheduling input. Validate the complete target set before
        // changing any membership or consuming the permit.
        let mut retire = Vec::new();
        if let SignalBoundaryOutcome::Terminated { group, .. } = receipt.outcome {
            let pid = DetTid::from_raw(receipt.permit.task.process.tgid.as_raw());
            let current = self.real_timers.task_identity(pid, tid);
            match current {
                Some((mm, identity))
                    if identity == receipt.permit.task && self.rpc_incarnation_matches(tid, mm) => {
                }
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
        }
        // Cleanup/failure may already have logically killed the task. Exact
        // duplicates remain recognizable after timer/task retirement. No turn
        // or membership is created by this consuming notification.
        self.parked.permits.remove(&tid);
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
