/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Actual scheduler fence glue with explicitly scripted native receipts.
//! These controls do not claim that a fake receipt proves ptrace completion.

use reverie::reserved_input::InputGrantKind;
use reverie::reserved_input::ReservedInputControl;

use super::Scheduler;
use crate::config::Config;
use crate::types::DetTid;

fn exec_authority_fixture() -> (
    Scheduler,
    ReservedInputControl,
    reverie::reserved_input::InputTaskId,
    DetTid,
    crate::types::MmId,
) {
    let control = ReservedInputControl::new_backend().unwrap();
    let native = control.backend_register_root(123, 123).unwrap();
    let tid = DetTid::from_raw(17);
    let mm = crate::types::MmId::initial(tid).for_exec(tid);
    control
        .backend_bind_tool_identity(native.task, tid.as_raw(), tid.as_raw())
        .unwrap();
    let mut scheduler = Scheduler::new(&Config::default());
    scheduler.reserved_input_control = Some(control.clone());
    scheduler.reserved_input_tool_tasks.insert(tid, native.task);
    scheduler
        .reserved_input_native_tasks
        .insert(123, native.task);
    scheduler.thread_tree.add_child(tid, tid, true);
    scheduler.record_thread_mm(tid, mm);
    scheduler.install_test_exec_incarnation(tid, mm);
    scheduler.register_thread_for_test(tid);
    scheduler.set_running_for_test(tid);
    (scheduler, control, native.task, tid, mm)
}

#[test]
fn exec_input_publication_requires_current_turn_and_exact_replacement_binding() {
    for invalid in 0..10 {
        let (mut scheduler, control, task, tid, mm) = exec_authority_fixture();
        let mut request_mm = mm;
        let mut process = tid;
        match invalid {
            0 => scheduler.set_running_for_test(DetTid::from_raw(18)),
            1 => scheduler.next_turns[&tid]
                .req
                .put(Ok(crate::resources::Resources::new(tid))),
            2 => {
                scheduler
                    .blocked
                    .external_io_blockers
                    .insert(tid, crate::resources::ExternalOpId::new(tid, 1));
            }
            3 => request_mm = mm.for_exec(tid),
            4 => process = DetTid::from_raw(18),
            5 => {
                scheduler.reserved_input_tool_tasks.remove(&tid);
            }
            6 => {
                scheduler.reserved_input_native_tasks.remove(&123);
            }
            7 => {
                scheduler.reserved_input_settled_tasks.insert(task);
            }
            8 => {
                control.freeze_tasks(&[task]).unwrap();
            }
            9 => scheduler.install_test_exec_incarnation(tid, mm.for_exec(tid)),
            _ => unreachable!(),
        }
        let before = (scheduler.turn, scheduler.committed_time);
        assert!(
            scheduler
                .publish_reserved_input_exec_authority(tid, process, request_mm)
                .is_err(),
            "invalid case {invalid} acquired authority"
        );
        assert_eq!(control.current_grant(task), None, "case {invalid}");
        assert_eq!((scheduler.turn, scheduler.committed_time), before);
    }
}

#[test]
fn exec_input_publication_preserves_an_existing_consumed_grant() {
    let (scheduler, control, task, tid, mm) = exec_authority_fixture();
    let before = (scheduler.turn, scheduler.committed_time);
    scheduler
        .publish_reserved_input_exec_authority(tid, tid, mm)
        .unwrap();
    let grant = control.current_grant(task).unwrap();
    let scope = control.begin_operation(task, 23).unwrap();
    scheduler
        .publish_reserved_input_exec_authority(tid, tid, mm)
        .unwrap();
    assert_eq!(control.current_grant(task), Some(grant));
    assert_eq!(
        control.begin_operation(task, 24).unwrap_err().reason(),
        "input-grant-unavailable"
    );
    assert_eq!((scheduler.turn, scheduler.committed_time), before);
    assert!(scheduler.next_turns[&tid].req.is_empty());
    control.finish_operation(scope).unwrap();
}

impl Scheduler {
    /// Script the physical-copy provenance used by the real post-exec
    /// take-back handler. This is a policy fixture, not a native signal send.
    pub(crate) fn install_exec_input_timer_take_back_for_test(&mut self, tid: DetTid) {
        self.discarded_at_send.lists.append_timer(
            super::discarded_at_send::QueueId::Thread(tid),
            libc::SIGUSR1,
            super::signal_lists::TimerEntry {
                process: self.registered_process(tid).unwrap(),
                timer: 1,
                gen_at_send: 1,
                periodic_at_send: false,
            },
            Some(false),
        );
        super::TIMER_COPIES_RECORDED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn reserved_input_fence_keeps_late_group_members_until_all_exact_fences_complete() {
    let control = ReservedInputControl::new_backend().unwrap();
    let parent = control.backend_register_root(10, 10).unwrap();
    let first = control
        .backend_register_child(parent.task, 11, 10, Some(true), Some(true))
        .unwrap();
    let cohort = control.freeze_group(parent.task, false).unwrap();
    let late = control
        .backend_register_child(parent.task, 12, 10, Some(true), Some(true))
        .unwrap();
    let mut scheduler = Scheduler::new(&Config::default());
    scheduler.reserved_input_operations =
        Some(crate::reserved_input::NativeOperations::install(&control).unwrap());
    scheduler.reserved_input_control = Some(control.clone());
    scheduler
        .reserved_input_tasks
        .insert(DetTid::from_raw(11), first.task);
    scheduler
        .reserved_input_tasks
        .insert(DetTid::from_raw(12), late.task);
    scheduler.freeze_reserved_input_thread(DetTid::from_raw(11));
    scheduler.freeze_reserved_input_thread(DetTid::from_raw(12));
    control.backend_terminal(first.task).unwrap();
    scheduler.settle_reserved_input_fence(&[DetTid::from_raw(11)]);
    assert_eq!(control.cohort_members(&cohort).unwrap().len(), 2);
    assert_eq!(
        control
            .grant(late.task, InputGrantKind::Ordinary)
            .unwrap_err()
            .reason(),
        "input-task-frozen"
    );
    control.backend_terminal(late.task).unwrap();
    scheduler.settle_reserved_input_fence(&[DetTid::from_raw(12)]);
    assert_eq!(
        control.cohort_members(&cohort).unwrap_err().reason(),
        "input-freeze-missing"
    );
    assert!(control.grant(parent.task, InputGrantKind::Ordinary).is_ok());
}

#[test]
fn reserved_input_fence_uses_the_retained_old_generation_not_a_reused_number() {
    let control = ReservedInputControl::new_backend().unwrap();
    let old = control.backend_register_root(11, 11).unwrap();
    let mut scheduler = Scheduler::new(&Config::default());
    let tid = DetTid::from_raw(11);
    scheduler.reserved_input_operations =
        Some(crate::reserved_input::NativeOperations::install(&control).unwrap());
    scheduler.reserved_input_control = Some(control.clone());
    scheduler.reserved_input_tasks.insert(tid, old.task);
    scheduler.freeze_reserved_input_thread(tid);
    control.backend_terminal(old.task).unwrap();
    let replacement = control.backend_register_root(11, 11).unwrap();
    assert_ne!(old.task, replacement.task);
    scheduler.reserved_input_tasks.insert(tid, replacement.task);
    scheduler.settle_reserved_input_fence(&[tid]);
    assert_eq!(control.native_terminal(replacement.task), Some(false));
    assert!(
        control
            .grant(replacement.task, InputGrantKind::Ordinary)
            .is_ok()
    );
    assert!(
        !scheduler
            .reserved_input_settled_tasks
            .contains(&replacement.task)
    );
}

#[test]
fn reserved_input_fence_never_silently_omits_a_missing_native_binding() {
    let mut scheduler = Scheduler::new(&Config::default());
    scheduler.reserved_input_control = Some(ReservedInputControl::new_backend().unwrap());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scheduler.settle_reserved_input_fence(&[DetTid::from_raw(44)]);
    }));
    assert!(result.is_err());
}

#[test]
fn reserved_input_fence_cannot_use_another_generations_terminal_receipt() {
    let control = ReservedInputControl::new_backend().unwrap();
    let old = control.backend_register_root(11, 11).unwrap();
    control.backend_terminal(old.task).unwrap();
    let replacement = control.backend_register_root(11, 11).unwrap();
    assert_ne!(old.task, replacement.task);
    let mut scheduler = Scheduler::new(&Config::default());
    let tid = DetTid::from_raw(11);
    scheduler.reserved_input_operations =
        Some(crate::reserved_input::NativeOperations::install(&control).unwrap());
    scheduler.reserved_input_control = Some(control.clone());
    scheduler.reserved_input_tasks.insert(tid, replacement.task);
    scheduler.freeze_reserved_input_thread(tid);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scheduler.settle_reserved_input_fence(&[tid]);
    }));
    assert!(result.is_err());
    assert!(
        !scheduler
            .reserved_input_settled_tasks
            .contains(&replacement.task)
    );
    assert_eq!(
        control
            .grant(replacement.task, InputGrantKind::Ordinary)
            .unwrap_err()
            .reason(),
        "input-task-frozen"
    );
}

#[test]
fn reserved_input_nonleader_exec_settles_old_images_before_binding_the_survivor() {
    let control = ReservedInputControl::new_backend().unwrap();
    let leader_native = control.backend_register_root(123, 123).unwrap();
    let caller_native = control
        .backend_register_child(leader_native.task, 124, 123, Some(true), Some(true))
        .unwrap();
    let leader = DetTid::from_raw(17);
    let caller = DetTid::from_raw(18);
    let mm = crate::types::MmId::initial(leader);
    let mut scheduler = Scheduler::new(&Config::default());
    scheduler.reserved_input_operations =
        Some(crate::reserved_input::NativeOperations::install(&control).unwrap());
    scheduler.reserved_input_control = Some(control.clone());
    for (tid, native) in [(leader, leader_native), (caller, caller_native)] {
        control
            .backend_bind_tool_identity(native.task, tid.as_raw(), leader.as_raw())
            .unwrap();
        scheduler.reserved_input_tool_tasks.insert(tid, native.task);
        scheduler.record_thread_mm(tid, mm);
        scheduler.register_thread_for_test(tid);
        scheduler.runqueue_push_back(tid);
    }
    scheduler.thread_tree.add_child(leader, leader, true);
    scheduler.thread_tree.add_child(leader, caller, false);
    // Explicitly scripted authenticated native Exec. The actual kernel seam
    // is a separate live/backend control, not proved by this state setup.
    let replacement = control
        .backend_exec_replacement(leader_native.task, caller_native.task)
        .unwrap();
    control
        .backend_bind_tool_identity(replacement.task, leader.as_raw(), leader.as_raw())
        .unwrap();
    scheduler
        .reserved_input_tool_tasks
        .insert(leader, replacement.task);
    assert_eq!(scheduler.reserved_input_tasks[&leader], leader_native.task);
    let retired = scheduler.reconnect_transferred_exec(super::ExecReconnect {
        caller,
        new_leader: leader,
        detpid: leader,
        pre_exec_mm: mm,
        post_exec_mm: mm.for_exec(leader),
        child_tid_addr: 0,
        reconnect_priority: Some(super::DEFAULT_PRIORITY),
    });
    assert_eq!(retired, vec![leader, caller]);
    assert_eq!(scheduler.reserved_input_tasks[&leader], replacement.task);
    assert!(
        scheduler
            .reserved_input_settled_tasks
            .contains(&leader_native.task)
    );
    assert!(
        scheduler
            .reserved_input_settled_tasks
            .contains(&caller_native.task)
    );
    assert!(
        !scheduler
            .reserved_input_settled_tasks
            .contains(&replacement.task)
    );
    assert_eq!(control.native_terminal(replacement.task), Some(false));
    assert!(
        control
            .grant(replacement.task, InputGrantKind::Ordinary)
            .is_ok()
    );
    assert_eq!(
        scheduler.turn, 0,
        "the fence/rebinding must not create a turn"
    );
}
