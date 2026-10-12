/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The successful exec edge changes a surviving thread's identity, not its clock.

use reverie::Errno;

use super::*;

/// Retained until the replacement proves it received the reconnect response.
/// In particular, a remote RPC can commit before its caller receives the reply.
/// This receipt authorizes consuming cleanup only; it never admits an ordinary
/// request from the former thread or the old address space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExecTransferReceipt {
    former: DetTid,
    process: DetPid,
    previous_mm: MmId,
    current_mm: MmId,
}

impl GlobalState {
    pub(super) async fn recv_exec_transfer(
        &self,
        from: Tid,
        time: DetTime,
        mm: MmId,
        former: DetTid,
        process: DetPid,
    ) -> <Self as GlobalTool>::Response {
        let current = DetTid::from_raw(from.as_raw());
        let mut sched = self.lock_rpc_scheduler(false).await;
        let mut pending = self.pending_exec_states.lock().unwrap();
        let Some(prepared) = pending.get(&process) else {
            return (None, GlobalResponse::ReconnectExec(false));
        };
        if current != process
            || former == current
            || prepared.caller != former
            || prepared.process != process
            || prepared.mm.for_exec(process) != mm
            || sched.registered_process(former) != Some(process)
            || sched.registered_process(current) != Some(process)
            || !sched.rpc_incarnation_matches(former, prepared.mm)
            || !sched.next_turns.contains_key(&former)
            || (self.cfg.sequentialize_threads
                && (sched.next_turns[&former].req.try_read().is_some()
                    || (sched.next_turns.contains_key(&current)
                        && !sched.exec_sibling_retirement_observed(current, process, prepared.mm))))
            || !self.global_time.lock().unwrap().contains_thread(former)
            || self
                .completed_exec_transfers
                .lock()
                .unwrap()
                .contains_key(&process)
        {
            return (None, GlobalResponse::ReconnectExec(false));
        }

        // The prepared caller's empty request keeps the scheduler quiescent.
        // Account its final pre-exec work and publish the entire replacement
        // before retiring that request, under the same mutex as turn grants.
        let prepared = pending.remove(&process).unwrap();
        let mut global_time = self.global_time.lock().unwrap();
        global_time.update_global_time(former, time.as_nanos(), time.inherited_nanos());
        sched.reconnect_transferred_exec(ExecReconnect {
            caller: former,
            new_leader: current,
            detpid: process,
            pre_exec_mm: prepared.mm,
            post_exec_mm: mm,
            child_tid_addr: 0,
            reconnect_priority: None,
        });
        global_time.reassign_thread(former, current);
        self.completed_exec_transfers.lock().unwrap().insert(
            process,
            ExecTransferReceipt {
                former,
                process,
                previous_mm: prepared.mm,
                current_mm: mm,
            },
        );
        if !prepared.fd_blocking.is_empty() {
            self.post_exec_fd_blocking
                .lock()
                .unwrap()
                .insert(current, prepared.fd_blocking);
        }
        (None, GlobalResponse::ReconnectExec(true))
    }

    /// Called only after ordinary incarnation and sender checks, with the
    /// scheduler mutex held. Such a request proves the local identity is bound.
    pub(super) fn acknowledge_exec_transfer(&self, sender: DetTid, mm: MmId) {
        let mut completed = self.completed_exec_transfers.lock().unwrap();
        if completed
            .get(&sender)
            .is_some_and(|receipt| receipt.current_mm == mm)
        {
            completed.remove(&sender);
        }
    }

    pub(super) async fn recv_retire_exec_transfer(
        &self,
        from: Tid,
        time: DetTime,
        mm: MmId,
        mut thread: ThreadDeregistration,
        signaled: bool,
    ) -> <Self as GlobalTool>::Response {
        let current = DetTid::from_raw(from.as_raw());
        let former = thread.dettid;
        let process = thread.detpid;
        let mut sched = self.lock_rpc_scheduler(true).await;
        // A surviving image cannot voluntarily exit before its post-exec
        // callback binds the transferred state. Ptrace cancellation supplies
        // a signal exit or a separately published backend failure. A pending
        // exec record alone cannot authorize an injected ordinary exit(0).
        if (!signaled && !sched.backend_failed())
            || current != process
            || former == current
            || thread.mm != mm
            || sched.registered_process(former) != Some(process)
            || sched.registered_process(current) != Some(process)
        {
            return (None, GlobalResponse::RetireExec(false));
        }
        let mut pending = self.pending_exec_states.lock().unwrap();
        let mut completed = self.completed_exec_transfers.lock().unwrap();
        let owner = if let Some(receipt) = completed.get(&process) {
            if receipt.former != former
                || receipt.process != process
                || receipt.current_mm != mm
                || receipt.previous_mm.for_exec(process) != mm
                || !sched.rpc_incarnation_matches(current, mm)
            {
                return (None, GlobalResponse::RetireExec(false));
            }
            thread.dettid = current;
            current
        } else if let Some(prepared) = pending.get(&process) {
            if prepared.caller != former
                || prepared.process != process
                || prepared.mm.for_exec(process) != mm
                || !sched.rpc_incarnation_matches(former, prepared.mm)
                || (self.cfg.sequentialize_threads
                    && sched.next_turns.contains_key(&current)
                    && !sched.exec_sibling_retirement_observed(current, process, prepared.mm))
            {
                return (None, GlobalResponse::RetireExec(false));
            }
            // No replacement registration was committed. Consume the original
            // owner in its registered address space without admitting new work.
            thread.mm = prepared.mm;
            former
        } else {
            return (None, GlobalResponse::RetireExec(false));
        };
        if !self.global_time.lock().unwrap().contains_thread(owner) {
            return (None, GlobalResponse::RetireExec(false));
        }
        if let Some(prepared) = pending.remove(&process) {
            // The authenticated backend sender has already taken over the
            // leader TID: kernel exec succeeded even if cancellation prevented
            // the reconnect reply. Retire the complete proven-dead cohort,
            // including peers whose physical cleanup callbacks arrive later.
            sched.finish_exec_teardown(prepared.caller, process, prepared.mm, true);
        }
        completed.remove(&process);
        self.global_time.lock().unwrap().update_global_time(
            owner,
            time.as_nanos(),
            time.inherited_nanos(),
        );
        self.account_deregistered_thread(&mut sched, thread);
        (None, GlobalResponse::RetireExec(true))
    }
}

/// Reconnect before any ordinary replacement-image callback work. The backend
/// supplies the current TID while preserving the former thread's entire state.
/// Neither newborn initialization nor a new PMU counter belongs on this edge.
pub(crate) async fn reconnect_exec<G, T>(guest: &mut G) -> Result<(), Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let current = DetTid::from_raw(guest.tid().as_raw());
    let former = guest.thread_state().dettid;
    if current == former {
        return Ok(());
    }
    let process = guest.thread_state().detpid.ok_or(Errno::EINVAL)?;
    let response = guest
        .send_rpc((
            guest.thread_state().thread_logical_time.clone(),
            guest.thread_state().mm_id,
            GlobalRequest::ReconnectExec { former, process },
        ))
        .await;
    if response != (None, GlobalResponse::ReconnectExec(true)) {
        return Err(Errno::EINVAL);
    }
    // There is no await between the acknowledgment and local binding. If the
    // transport loses the reply, consuming cleanup uses the completed receipt.
    guest.thread_state_mut().dettid = current;
    // Preemption artifacts have one history per raw TID, with no exec
    // incarnation. Appending this worker's PMU targets to the displaced
    // leader's history is invalid; replay would still read the worker's tail.
    // Complete the authenticated identity/accounting handoff so shutdown owns
    // the right task, but refuse before requesting a replacement-image turn.
    // Ordinary chaos execution without these artifact modes still requests
    // its normal continuation below.
    if guest.config().record_preemptions
        || guest.config().record_preemptions_to.is_some()
        || guest.config().replay_preemptions_from.is_some()
    {
        return Err(Errno::EOPNOTSUPP);
    }
    if guest.config().sequentialize_threads {
        let (_, response) = send_and_update_time(guest, GlobalRequest::ResumeExec(process)).await;
        let GlobalResponse::ResumeExec(_, duration) = response else {
            unreachable!("exec continuation must receive its typed grant");
        };
        if let Some(duration) = duration {
            let maximum_enabled = guest.config().max_timeslice.is_some();
            let thread = guest.thread_state_mut();
            let deadline = thread.thread_logical_time.as_nanos() + duration;
            thread.end_of_timeslice = Some(deadline);
            if maximum_enabled {
                thread.max_timeslice_end = Some(deadline);
            }
        }
        // The continuation's slice end, just installed, takes the grant's
        // deadline cap like any other
        // (<https://github.com/rrnewton/hermit/issues/4121>).
        let deadline = guest.thread_state_mut().last_grant_deadline.take();
        super::cap_timeslice_at_deadline(guest, deadline);
    }
    Ok(())
}

pub(crate) async fn retire_exec<R: GlobalRPC<GlobalState>>(
    time: DetTime,
    rpc: &R,
    thread: ThreadDeregistration,
    signaled: bool,
) -> Result<(), Errno> {
    let response = rpc
        .send_rpc((
            time,
            thread.mm,
            GlobalRequest::RetireExec { thread, signaled },
        ))
        .await;
    if response == (None, GlobalResponse::RetireExec(true)) {
        Ok(())
    } else {
        Err(Errno::EINVAL)
    }
}
