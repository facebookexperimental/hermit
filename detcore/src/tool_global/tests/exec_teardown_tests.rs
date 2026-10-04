/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt::Write;
use std::sync::Arc;
use std::task::Poll;

use super::*;
use crate::types::ExactChildWaitState;

const PARENT: DetTid = DetTid::from_raw(1);
const LEADER: DetTid = DetTid::from_raw(17);
const WORKER: DetTid = DetTid::from_raw(19);

fn state(tids: &[DetTid]) -> (GlobalState, DetTime) {
    let config = Config {
        sequentialize_threads: true,
        cancel_killed_thread_rpcs: false,
        ..Config::default()
    };
    let state = GlobalState::initialize(&config, false);
    {
        let mut sched = state.sched.lock().unwrap();
        sched.thread_tree.add_child(PARENT, PARENT, true);
        sched.thread_tree.add_child(PARENT, LEADER, true);
        for tid in tids.iter().copied().filter(|tid| *tid != LEADER) {
            sched.thread_tree.add_child(LEADER, tid, false);
        }
    }
    let time = DetTime::new(&config);
    for tid in tids {
        install_test_registration(&state, *tid, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .install_test_exec_incarnation(*tid, MmId::initial(LEADER));
        state.global_time.lock().unwrap().update_global_time(
            *tid,
            time.as_nanos(),
            time.inherited_nanos(),
        );
    }
    (state, time)
}

fn deregistration(tid: DetTid, mm: MmId, count: u64) -> ThreadDeregistration {
    let mut stats = TimesliceStats::default();
    stats.record(41);
    ThreadDeregistration {
        dettid: tid,
        detpid: LEADER,
        mm,
        thread_start_entered: true,
        timeslice_stats: stats,
        syscall_count: count,
        chaos_epochs: Vec::new(),
    }
}

#[tokio::test]
async fn subsequent_leader_exec_cancellation_consumes_prepared_incarnation() {
    let (state, mut time) = state(&[LEADER]);
    let mm0 = MmId::initial(LEADER);
    let mm1 = mm0.for_exec(LEADER);
    let mm2 = mm1.for_exec(LEADER);
    let sender = Tid::from_raw(LEADER.as_raw());

    // Bind mm1 through the actual successful leader-exec RPC, then prepare a
    // second exec. Cancellation comes after local state selected candidate mm2.
    for (mm, request, expected) in [
        (
            mm0,
            GlobalRequest::PrepareExec(LEADER, mm0, Default::default()),
            GlobalResponse::PrepareExec(()),
        ),
        (
            mm1,
            GlobalRequest::MarkPastFirstExecve(LEADER, None),
            GlobalResponse::MarkPastFirstExecve(Default::default()),
        ),
        (
            mm1,
            GlobalRequest::PrepareExec(LEADER, mm1, Default::default()),
            GlobalResponse::PrepareExec(()),
        ),
    ] {
        assert_eq!(
            state.receive_rpc(sender, (time.clone(), mm, request)).await,
            (None, expected)
        );
    }
    {
        let sched = state.sched.lock().unwrap();
        assert!(sched.rpc_incarnation_matches(LEADER, mm1));
        assert!(!sched.rpc_incarnation_matches(LEADER, mm2));
        assert!(sched.next_turns.contains_key(&LEADER));
    }
    let before = state.global_time.lock().unwrap().as_nanos();
    time.add_syscall_with_cost(123);
    let mut cleanup = Box::pin(state.receive_rpc(
        sender,
        (
            time,
            mm2,
            GlobalRequest::DeregisterThread(deregistration(LEADER, mm2, 9)),
        ),
    ));
    assert_eq!(
        futures::poll!(&mut cleanup),
        Poll::Ready((None, GlobalResponse::DeregisterThread(())))
    );
    assert!(state.pending_exec_states.lock().unwrap().is_empty());
    let mut sched = state.sched.lock().unwrap();
    assert!(
        !sched.next_turns.contains_key(&LEADER),
        "candidate-mm cleanup must not strand the registered caller"
    );
    assert!(!sched.priorities.contains_key(&LEADER));
    assert_eq!(sched.per_thread_syscalls.get(&LEADER), Some(&9));
    assert_eq!(
        sched.per_thread_timeslice.get(&LEADER),
        Some(&deregistration(LEADER, mm1, 9).timeslice_stats)
    );
    assert_eq!(
        sched.exact_child_wait_state(PARENT, LEADER),
        ExactChildWaitState::LogicallyExited
    );
    assert_eq!(sched.turn, 0);
    assert_eq!(
        state.global_time.lock().unwrap().as_nanos(),
        before + LogicalTime::from_nanos(123)
    );
}

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
        if *event.metadata().level() != tracing::Level::INFO {
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

#[tokio::test]
async fn cancelled_transferred_exec_retires_unobserved_peer_before_late_callback() {
    if !crate::scheduler::exec_teardown_tests::in_isolated_log_test(
        module_path!(),
        "cancelled_transferred_exec_retires_unobserved_peer_before_late_callback",
    ) {
        return;
    }
    let early_peer = DetTid::from_raw(23);
    let late_peer = DetTid::from_raw(18);
    let (state, time) = state(&[LEADER, WORKER, early_peer, late_peer]);
    let mm = MmId::initial(LEADER);
    let candidate = mm.for_exec(LEADER);
    assert_eq!(
        state
            .receive_rpc(
                Tid::from_raw(WORKER.as_raw()),
                (
                    time.clone(),
                    mm,
                    GlobalRequest::PrepareExec(LEADER, mm, Default::default())
                ),
            )
            .await,
        (None, GlobalResponse::PrepareExec(()))
    );
    let records = InfoRecords::default();
    let _capture = tracing::subscriber::set_default(records.clone());
    for (tid, count) in [(early_peer, 5), (LEADER, 3)] {
        assert_eq!(
            state
                .receive_rpc(
                    Tid::from_raw(tid.as_raw()),
                    (
                        time.clone(),
                        mm,
                        GlobalRequest::DeregisterThread(deregistration(tid, mm, count))
                    ),
                )
                .await,
            (None, GlobalResponse::DeregisterThread(()))
        );
    }
    assert!(records.0.lock().unwrap().is_empty());
    {
        let sched = state.sched.lock().unwrap();
        assert!(sched.exec_sibling_retirement_observed(LEADER, LEADER, mm));
        assert!(!sched.exec_sibling_retirement_observed(late_peer, LEADER, mm));
        assert!(sched.next_turns.contains_key(&late_peer));
    }
    let mut cleanup = Box::pin(state.receive_rpc(
        Tid::from_raw(LEADER.as_raw()),
        (
            time.clone(),
            candidate,
            GlobalRequest::RetireExec {
                thread: deregistration(WORKER, candidate, 7),
                signaled: true,
            },
        ),
    ));
    assert_eq!(
        futures::poll!(&mut cleanup),
        Poll::Ready((None, GlobalResponse::RetireExec(true)))
    );
    let expected: Vec<_> = [LEADER, late_peer, early_peer, WORKER].into_iter().map(|tid| {
        format!("logically_kill: Scheduler removing all knowledge of [det]tid {tid} in pid {LEADER}..")
    }).collect();
    assert_eq!(*records.0.lock().unwrap(), expected);
    {
        let mut sched = state.sched.lock().unwrap();
        for tid in [LEADER, WORKER, early_peer, late_peer] {
            assert!(!sched.next_turns.contains_key(&tid));
        }
        assert_eq!(sched.per_thread_syscalls.get(&WORKER), Some(&7));
        assert!(!sched.per_thread_syscalls.contains_key(&late_peer));
        assert_eq!(
            sched.exact_child_wait_state(PARENT, LEADER),
            ExactChildWaitState::LogicallyExited
        );
        assert_eq!(sched.turn, 0);
    }
    assert!(state.pending_exec_states.lock().unwrap().is_empty());
    assert!(state.completed_exec_transfers.lock().unwrap().is_empty());
    // The late physical owner still contributes its final stats, without
    // performing another logical removal or adding an arrival-ordered INFO.
    assert_eq!(
        state
            .receive_rpc(
                Tid::from_raw(late_peer.as_raw()),
                (
                    time,
                    mm,
                    GlobalRequest::DeregisterThread(deregistration(late_peer, mm, 11))
                ),
            )
            .await,
        (None, GlobalResponse::DeregisterThread(()))
    );
    assert_eq!(*records.0.lock().unwrap(), expected);
    let sched = state.sched.lock().unwrap();
    assert_eq!(sched.per_thread_syscalls.get(&late_peer), Some(&11));
    assert_eq!(sched.per_thread_syscalls.values().sum::<u64>(), 26);
    assert_eq!(
        sched
            .per_thread_timeslice
            .values()
            .map(|stats| stats.count)
            .sum::<u64>(),
        4
    );
    assert_eq!(sched.turn, 0);
}
