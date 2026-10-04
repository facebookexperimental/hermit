/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use super::*;

#[derive(Clone, Default)]
struct InfoRecords(Arc<Mutex<Vec<String>>>);

impl tracing::Subscriber for InfoRecords {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() != Level::INFO {
            return;
        }
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    write!(self.0, "{value:?}").unwrap();
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0.lock().unwrap().push(message.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn register(sched: &mut Scheduler, tid: DetTid) {
    sched.priorities.insert(tid, DEFAULT_PRIORITY);
    sched.next_turns.insert(
        tid,
        ThreadNextTurn {
            dettid: tid,
            child_tid_addr: 0,
            req: Ivar::new(),
            resp: Ivar::new(),
            protocol: Default::default(),
        },
    );
    sched.runqueue_push_back(tid);
}

fn group() -> (Scheduler, DetPid, MmId) {
    let mut sched = Scheduler::new(&Config::default());
    let process = DetPid::from_raw(17);
    sched.thread_tree.add_child(process, process, true);
    register(&mut sched, process);
    for raw in [18, 19, 23] {
        let tid = DetTid::from_raw(raw);
        sched.thread_tree.add_child(process, tid, false);
        register(&mut sched, tid);
    }
    // Historical tree membership is not part of a live exec cohort.
    sched
        .thread_tree
        .add_child(process, DetTid::from_raw(13), false);
    let unrelated = DetTid::from_raw(31);
    sched.thread_tree.add_child(process, unrelated, true);
    register(&mut sched, unrelated);
    sched.turn = 41;
    sched.committed_time = LogicalTime::from_nanos(17_000);
    (sched, process, MmId::initial(process))
}

fn reconnect(sched: &mut Scheduler, caller: DetTid, process: DetPid, mm: MmId) {
    let args = ExecReconnect {
        caller,
        new_leader: process,
        detpid: process,
        pre_exec_mm: mm,
        post_exec_mm: mm.for_exec(process),
        child_tid_addr: 0,
        reconnect_priority: None,
    };
    if caller == process {
        sched.reconnect_after_exec(args);
    } else {
        sched.reconnect_transferred_exec(args);
    }
}

fn retirement_records(tids: &[DetTid], process: DetPid) -> Vec<String> {
    tids.iter()
        .map(|tid| {
            format!(
                "logically_kill: Scheduler removing all knowledge of [det]tid {tid} in pid {process}.."
            )
        })
        .collect()
}

pub(crate) fn in_isolated_log_test(module: &str, name: &str) -> bool {
    const CHILD_TEST: &str = "HERMIT_EXEC_TEARDOWN_LOG_TEST";
    let module = module.split_once("::").unwrap().1;
    let test = format!("{module}::{name}");
    if std::env::var(CHILD_TEST).as_deref() == Ok(test.as_str()) {
        return true;
    }
    // Tracing caches callsite interest process-wide. Parallel tests can visit
    // the same scheduler callsite with NoSubscriber while our thread-local
    // capture is being registered, caching `never` and losing real records.
    // Run this exact oracle once in its own process; do not serialize the
    // library suite, retry it, or replace the production INFO event with a
    // test-only observation.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test, "--nocapture"])
        .env(CHILD_TEST, &test)
        .output()
        .expect("launch isolated exec teardown log test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed; 0 failed;"),
        "isolated test {test} did not pass exactly once: {}\n{stdout}\n{stderr}",
        output.status
    );
    false
}

#[test]
fn nonleader_exec_transfers_child_wait_owner_without_changing_wait_class() {
    for preserves_state in [false, true] {
        let (mut sched, process, mm) = group();
        let caller = DetTid::from_raw(19);
        let normal = DetPid::from_raw(41);
        let clone_child = DetPid::from_raw(42);
        let leader_child = DetPid::from_raw(31);
        for (child, exit_signal) in [(normal, libc::SIGCHLD), (clone_child, 0)] {
            sched
                .thread_tree
                .add_child_with_wait_metadata(caller, child, true, false, exit_signal);
            sched.logically_exited_processes.insert(child);
        }
        let wait = |child, owner, exit_class| ChildWaitSpec {
            selector: ChildWaitSelector::Exact(child),
            owner,
            exit_class,
        };
        for child in [normal, clone_child] {
            assert_eq!(
                sched
                    .ready_child_wait(process, wait(child, Some(caller), ChildWaitExitClass::Any),),
                Some(child),
            );
            assert!(!sched.has_child_wait_target(
                process,
                wait(child, Some(process), ChildWaitExitClass::Any),
            ));
        }

        assert!(sched.prepare_exec_teardown(caller, process, mm));
        let args = ExecReconnect {
            caller,
            new_leader: process,
            detpid: process,
            pre_exec_mm: mm,
            post_exec_mm: mm.for_exec(process),
            child_tid_addr: 0,
            reconnect_priority: None,
        };
        let before = (sched.turn, sched.committed_time);
        if preserves_state {
            sched.reconnect_transferred_exec(args);
        } else {
            sched.reconnect_after_exec(args);
        }
        assert_eq!((sched.turn, sched.committed_time), before);

        for (child, class, excluded) in [
            (
                normal,
                ChildWaitExitClass::Sigchld,
                ChildWaitExitClass::Clone,
            ),
            (
                clone_child,
                ChildWaitExitClass::Clone,
                ChildWaitExitClass::Sigchld,
            ),
        ] {
            assert_eq!(
                sched.ready_child_wait(process, wait(child, Some(process), class)),
                Some(child),
                "the replacement task retains the survivor's waitable child",
            );
            assert_eq!(
                sched.ready_child_wait(process, wait(child, None, class)),
                Some(child),
                "process-wide wait selection remains unchanged",
            );
            assert!(!sched.has_child_wait_target(process, wait(child, Some(caller), class)));
            assert!(!sched.has_child_wait_target(process, wait(child, Some(process), excluded),));
            assert_eq!(sched.thread_tree.parent_process(&child), Some(process));
        }
        assert!(sched.has_child_wait_target(
            process,
            wait(leader_child, Some(process), ChildWaitExitClass::Sigchld),
        ));
        assert_eq!(
            sched.ready_child_wait(
                process,
                wait(leader_child, Some(process), ChildWaitExitClass::Sigchld),
            ),
            None,
            "changing ownership must not make a live child ready",
        );
    }
}

#[test]
fn exec_wait_owner_transfer_preserves_other_parents_and_creation_lineage() {
    let mut tree = ThreadTree::default();
    let root = DetPid::from_raw(1);
    let process = DetPid::from_raw(17);
    let former = DetTid::from_raw(19);
    let leader_child = DetPid::from_raw(31);
    let own_child = DetPid::from_raw(41);
    let grandchild = DetPid::from_raw(42);
    let clone_parent_child = DetPid::from_raw(43);
    let other_parent = DetPid::from_raw(51);
    let older_child = DetPid::from_raw(52);
    tree.add_child(root, root, true);
    tree.add_child(root, other_parent, true);
    tree.add_child(other_parent, former, false);
    tree.add_child(former, older_child, true);
    // ThreadTree retains old creation edges when a raw TID is registered in
    // another process. Its earlier child's effective parent remains distinct.
    tree.add_child(root, process, true);
    tree.add_child(process, former, false);
    tree.add_child(process, leader_child, true);
    tree.add_child(former, own_child, true);
    tree.add_child(own_child, grandchild, true);
    tree.add_child_with_wait_metadata(former, clone_parent_child, true, true, libc::SIGCHLD);
    assert!(tree.set_process_group(own_child, DetPid::from_raw(77)));
    assert!(tree.create_session(clone_parent_child));
    let before = tree.clone();

    tree.transfer_exec_wait_owner(process, former, process);

    assert_eq!(tree.process_wait[&own_child].wait_owner, process);
    assert_eq!(tree.process_wait[&leader_child].wait_owner, process);
    assert_eq!(tree.process_wait[&older_child].wait_owner, former);
    assert_eq!(tree.process_wait[&clone_parent_child].wait_owner, root);
    assert_eq!(tree.process_wait[&grandchild].wait_owner, own_child);
    assert_eq!(tree.process_parent, before.process_parent);
    assert_eq!(tree.tree, before.tree);
    assert_eq!(tree.thread_to_leader, before.thread_to_leader);
    assert_eq!(tree.process_wait.len(), before.process_wait.len());
    for (child, metadata) in &tree.process_wait {
        let previous = before.process_wait[child];
        assert_eq!(
            (
                metadata.wait_parent,
                metadata.exit_signal,
                metadata.process_group,
                metadata.session,
            ),
            (
                previous.wait_parent,
                previous.exit_signal,
                previous.process_group,
                previous.session,
            ),
        );
    }
}

#[test]
fn exec_teardown_orders_real_retirements_for_all_hook_permutations() {
    if !in_isolated_log_test(
        module_path!(),
        "exec_teardown_orders_real_retirements_for_all_hook_permutations",
    ) {
        return;
    }
    for caller in [DetTid::from_raw(17), DetTid::from_raw(19)] {
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            // Successful exec can precede any, some, or all sibling callbacks.
            for early in 0..=3 {
                let (mut sched, process, mm) = group();
                let siblings: Vec<_> = [17, 18, 19, 23]
                    .into_iter()
                    .map(DetTid::from_raw)
                    .filter(|tid| *tid != caller)
                    .collect();
                let caller_request = sched.next_turns[&caller].req.clone();
                let before = (sched.turn, sched.committed_time);
                assert!(sched.prepare_exec_teardown(caller, process, mm));
                let records = InfoRecords::default();
                tracing::subscriber::with_default(records.clone(), || {
                    for index in order.into_iter().take(early) {
                        let sibling = siblings[index];
                        assert!(sched.defer_exec_sibling_retirement(sibling, process, mm));
                        assert!(sched.exec_sibling_retirement_observed(sibling, process, mm));
                        assert!(sched.next_turns.contains_key(&sibling));
                        assert!(caller_request.try_read().is_none());
                    }
                    assert!(records.0.lock().unwrap().is_empty());
                    reconnect(&mut sched, caller, process, mm);
                    let mut retired = siblings.clone();
                    if caller != process {
                        retired.push(caller);
                    }
                    assert_eq!(
                        *records.0.lock().unwrap(),
                        retirement_records(&retired, process)
                    );
                    // Late hooks follow normal incarnation admission. The old
                    // leader cannot remove its replacement; other consumed
                    // siblings perform no second logical removal or INFO event.
                    for sibling in siblings.iter().copied() {
                        if sched.rpc_incarnation_matches(sibling, mm) {
                            assert!(!sched.defer_exec_sibling_retirement(sibling, process, mm));
                            sched.logically_kill_thread(&sibling, &process, mm);
                        }
                    }
                    assert_eq!(
                        *records.0.lock().unwrap(),
                        retirement_records(&retired, process)
                    );
                });
                assert_eq!((sched.turn, sched.committed_time), before);
                assert!(sched.next_turns[&process].req.try_read().is_none());
                assert!(sched.next_turns[&process].resp.try_read().is_none());
                assert!(sched.next_turns.contains_key(&DetTid::from_raw(31)));
                assert!(!sched.logically_exited_processes.contains(&process));
                assert_eq!(caller_request.try_read().is_some(), caller != process);
                assert!(sched.exec_teardowns.is_empty());
            }
        }
    }
}

#[test]
fn exec_teardown_abort_retires_only_observed_siblings_in_fixed_order() {
    if !in_isolated_log_test(
        module_path!(),
        "exec_teardown_abort_retires_only_observed_siblings_in_fixed_order",
    ) {
        return;
    }
    for order in [[18, 23], [23, 18]] {
        let (mut sched, process, mm) = group();
        assert!(sched.prepare_exec_teardown(process, process, mm));
        assert!(!sched.prepare_exec_teardown(process, process, mm));
        for raw in order {
            assert!(sched.defer_exec_sibling_retirement(DetTid::from_raw(raw), process, mm));
        }
        for (tid, pid, receipt_mm) in [
            (process, process, mm),
            (DetTid::from_raw(13), process, mm),
            (DetTid::from_raw(31), process, mm),
            (DetTid::from_raw(18), DetPid::from_raw(31), mm),
            (DetTid::from_raw(18), process, mm.for_exec(process)),
        ] {
            assert!(!sched.defer_exec_sibling_retirement(tid, pid, receipt_mm));
            assert!(!sched.exec_sibling_retirement_observed(tid, pid, receipt_mm));
        }
        assert!(
            sched
                .finish_exec_teardown(process, process, mm.for_exec(process), false)
                .is_empty()
        );
        assert!(sched.exec_sibling_retirement_observed(DetTid::from_raw(18), process, mm));
        let records = InfoRecords::default();
        let retired = tracing::subscriber::with_default(records.clone(), || {
            sched.finish_exec_teardown(process, process, mm, false)
        });
        assert_eq!(retired, [DetTid::from_raw(18), DetTid::from_raw(23)]);
        assert_eq!(
            *records.0.lock().unwrap(),
            retirement_records(&retired, process)
        );
        assert!(sched.next_turns.contains_key(&process));
        assert!(sched.next_turns.contains_key(&DetTid::from_raw(19)));
        assert!(sched.next_turns.contains_key(&DetTid::from_raw(31)));
        assert!(sched.next_turns[&process].req.try_read().is_none());
        assert!(
            sched
                .finish_exec_teardown(process, process, mm, false)
                .is_empty()
        );
        assert!(sched.prepare_exec_teardown(process, process, mm));
        assert!(
            sched
                .finish_exec_teardown(process, process, mm, false)
                .is_empty()
        );
        assert!(sched.next_turns.contains_key(&DetTid::from_raw(19)));
    }
}

#[test]
fn exec_teardown_preserves_tentative_queue_and_caller_fence() {
    for caller in [DetTid::from_raw(17), DetTid::from_raw(19)] {
        let (mut sched, process, mm) = group();
        let unrelated = DetTid::from_raw(31);
        let caller_request = sched.next_turns[&caller].req.clone();
        assert!(sched.prepare_exec_teardown(caller, process, mm));
        assert_eq!(
            sched.run_queue.tentative_pop_tid(unrelated),
            Some(unrelated)
        );
        let queue = sched.run_queue.tids().copied().collect::<Vec<_>>();
        for tid in [17, 18, 19, 23].into_iter().map(DetTid::from_raw) {
            if tid != caller {
                assert!(sched.defer_exec_sibling_retirement(tid, process, mm));
            }
        }
        assert!(caller_request.try_read().is_none());
        assert!(sched.pending_run_queue_removals.is_empty());
        reconnect(&mut sched, caller, process, mm);
        assert!(sched.run_queue.tentative_pop_in_progress());
        assert_eq!(sched.run_queue.tids().copied().collect::<Vec<_>>(), queue);
        assert!(sched.next_turns[&process].req.try_read().is_none());
        assert_eq!(caller_request.try_read().is_some(), caller != process);
        sched.run_queue.undo_tentative_pop();
        sched.drain_pending_run_queue_removals();
        sched.drain_pending_run_queue_admissions();
        assert_eq!(
            sched.run_queue.tids().copied().collect::<BTreeSet<_>>(),
            [process, unrelated].into()
        );
        assert_eq!(sched.turn, 41);
        assert_eq!(sched.committed_time, LogicalTime::from_nanos(17_000));
    }
}

#[test]
fn transferred_exec_tid_reuse_requires_registration_and_rebinds_incarnation() {
    for same_mm in [false, true] {
        let (mut sched, process, old_mm) = group();
        let former = DetTid::from_raw(19);
        reconnect(&mut sched, former, process, old_mm);
        sched.drain_pending_run_queue_removals();
        sched.drain_pending_run_queue_admissions();
        let new_mm = if same_mm {
            old_mm
        } else {
            old_mm.for_exec(process)
        };
        assert!(!sched.rpc_incarnation_matches(former, old_mm));
        assert!(!sched.rpc_incarnation_matches(former, new_mm));
        sched.deregistration_accounted.insert(former);
        sched.register_reused_transferred_exec_tid(former, new_mm);
        sched.thread_tree.add_child(process, former, false);
        register(&mut sched, former);
        assert!(sched.rpc_incarnation_matches(former, new_mm));
        assert_eq!(sched.rpc_incarnation_matches(former, old_mm), same_mm);
        assert!(!sched.rpc_incarnation_matches(former, new_mm.for_exec(process)));
        assert!(!sched.deregistration_accounted.contains(&former));
        assert!(sched.note_deregistration_accounted(former));
        // A repeated registration notification does not reset accounting on
        // the now-live replacement; only consuming the former marker did so.
        sched.register_reused_transferred_exec_tid(former, new_mm);
        assert!(sched.deregistration_accounted.contains(&former));
        assert!(sched.next_turns.contains_key(&former));
        assert!(!sched.thread_is_logically_killed(former));
    }
}

#[test]
fn reused_vfork_tid_requires_exact_pending_grant_and_consumes_its_authority() {
    for shares_vm in [false, true] {
        for completion in ["registered", "failed", "parent_exit"] {
            let (mut sched, parent, _) = group();
            let child = DetTid::from_raw(41);
            // The parent may itself share another process's address space.
            // Authority must come from the grant, not initial(parent_pid).
            let parent_mm = MmId::initial(DetPid::from_raw(59)).for_exec(DetTid::from_raw(59));
            let child_mm = MmId::for_clone(parent_mm, child, shares_vm);
            let op = ExternalOpId::new(parent, 7);
            let mut resources = Resources::new(parent);
            resources.insert(ResourceID::BlockingVfork(op), Permission::RW);
            let next = sched.next_turns.get_mut(&parent).unwrap();
            next.req.put(Ok(resources.clone()));
            next.protocol.origin = Some(parked::ResourceOrigin {
                rpc: parked::RpcOrigin::DirectRequestResources,
                mm: parent_mm,
                control: parked::ControlCapability::None,
            });
            let response = next.resp.clone();
            sched.retired_transferred_exec_callers.insert(child);
            assert!(
                !sched
                    .pending_vfork_registration_matches(parent, parent, child, child_mm, shares_vm)
            );
            assert_eq!(sched.run_queue.tentative_pop_tid(parent), Some(parent));
            assert!(
                sched
                    .step4_resource_block(parent, &resources, &response)
                    .is_err()
            );
            assert!(response.try_read().is_some());
            assert!(sched.next_turns[&parent].protocol.origin.is_none());
            assert!(
                sched
                    .pending_vfork_registration_matches(parent, parent, child, child_mm, shares_vm)
            );
            assert!(sched.transferred_exec_tid_requires_registration(child));
            assert!(!sched.rpc_incarnation_matches(child, child_mm));
            for (candidate_parent, process, candidate_child, candidate_mm) in [
                (DetTid::from_raw(18), parent, child, child_mm),
                (parent, DetPid::from_raw(31), child, child_mm),
                (parent, parent, parent, child_mm),
                (parent, parent, DetTid::from_raw(31), child_mm),
                (parent, parent, child, child_mm.for_exec(child)),
            ] {
                assert!(!sched.pending_vfork_registration_matches(
                    candidate_parent,
                    process,
                    candidate_child,
                    candidate_mm,
                    shares_vm,
                ));
            }
            match completion {
                "registered" => {
                    sched.register_reused_transferred_exec_tid(child, child_mm);
                    sched.complete_vfork_registration(parent, child);
                    assert!(sched.rpc_incarnation_matches(child, child_mm));
                    assert!(!sched.transferred_exec_tid_requires_registration(child));
                }
                "failed" => {
                    let mut failed = Resources::new(parent);
                    failed.insert(ResourceID::VforkFailed(op), Permission::RW);
                    sched.next_turns[&parent].req.put(Ok(failed));
                    assert!(sched.step2a_wait_for_vfork_barrier().is_ok());
                    assert!(sched.transferred_exec_tid_requires_registration(child));
                }
                "parent_exit" => {
                    sched.logically_kill_thread(&parent, &parent, parent_mm);
                    assert!(sched.transferred_exec_tid_requires_registration(child));
                }
                _ => unreachable!(),
            }
            assert!(!sched.vfork_registration_origins.contains_key(&parent));
            assert!(
                !sched
                    .pending_vfork_registration_matches(parent, parent, child, child_mm, shares_vm)
            );
        }
    }
}
