/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::SystemTime;

use futures::Future;
use reverie::ExitStatus;
use reverie::Tool;

use super::*;

fn failure(tid: DetTid) -> reverie::BackendFailure {
    reverie::BackendFailure {
        pid: Tid::from_raw(tid.as_raw()),
        tid: Tid::from_raw(tid.as_raw()),
        phase: "native consuming cleanup control",
    }
}

struct WakeCount(AtomicUsize);

impl futures::task::ArcWake for WakeCount {
    fn wake_by_ref(this: &Arc<Self>) {
        this.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn backend_failure_wakes_each_global_subscriber_and_late_arrivals() {
    let (_, state, tid, _) = cancellation_test_state();
    let mut first = std::pin::pin!(state.wait_for_backend_failure());
    let mut second = std::pin::pin!(state.wait_for_backend_failure());
    let first_wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
    let second_wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
    let first_waker = futures::task::waker(first_wakes.clone());
    let second_waker = futures::task::waker(second_wakes.clone());
    let mut first_context = Context::from_waker(&first_waker);
    let mut second_context = Context::from_waker(&second_waker);
    assert!(first.as_mut().poll(&mut first_context).is_pending());
    assert!(second.as_mut().poll(&mut second_context).is_pending());
    state.report_backend_failure(failure(tid));
    assert!(first_wakes.0.load(Ordering::SeqCst) > 0);
    assert!(second_wakes.0.load(Ordering::SeqCst) > 0);
    assert!(first.as_mut().poll(&mut first_context).is_ready());
    assert!(second.as_mut().poll(&mut second_context).is_ready());
    let mut late = std::pin::pin!(state.wait_for_backend_failure());
    assert!(late.as_mut().poll(&mut first_context).is_ready());
    state.report_backend_failure(failure(DetTid::from_raw(99)));
    assert!(state.sched.lock().unwrap().backend_failed());
}

// The transport only binds the real sender. The production GlobalState owns
// replies, clock updates, registration checks and consuming deregistration.
struct CleanupRpc<'a> {
    state: &'a GlobalState,
    tid: DetTid,
    calls: AtomicUsize,
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for CleanupRpc<'_> {
    async fn send_rpc(
        &self,
        request: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        assert!(matches!(request.2, GlobalRequest::DeregisterThread(_)));
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.state
            .receive_rpc(Tid::from_raw(self.tid.as_raw()), request)
            .await
    }

    fn config(&self) -> &Config {
        &self.state.cfg
    }
}

async fn consume_thread(state: &GlobalState, tool: &Detcore, thread: crate::ThreadState<()>) {
    let rpc = CleanupRpc {
        state,
        tid: thread.dettid,
        calls: AtomicUsize::new(0),
    };
    let mut completion = std::pin::pin!(tool.on_exit_thread(
        Tid::from_raw(thread.dettid.as_raw()),
        &rpc,
        thread,
        ExitStatus::Exited(0),
    ));
    assert!(matches!(
        futures::poll!(completion.as_mut()),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(rpc.calls.load(Ordering::SeqCst), 1);
}

async fn selected_cleanup(daemon_first: bool) {
    let (config, state, leader, _) = cancellation_test_state();
    let tool = Detcore::new(Tid::from_raw(leader.as_raw()), &config);
    let mut thread = tool.init_thread_state(Tid::from_raw(leader.as_raw()), None);
    thread.detpid = Some(leader);
    thread.thread_logical_time.add_syscall_with_cost(37);
    let waiter = DetTid::from_raw(19);
    let selected = DetTid::from_raw(23);
    let address = 0x404100;
    let futex = FutexID::private(thread.mm_id, address);
    let waiter_response;
    {
        let mut sched = state.sched.lock().unwrap();
        for tid in [waiter, selected] {
            sched.thread_tree.add_child(leader, tid, false);
        }
    }
    install_test_registration(&state, leader, Ivar::new());
    install_test_registration(&state, selected, Ivar::new());
    {
        let mut sched = state.sched.lock().unwrap();
        sched.next_turns.get_mut(&leader).unwrap().child_tid_addr = address;
        sched.next_turns.insert(
            waiter,
            ThreadNextTurn {
                dettid: waiter,
                child_tid_addr: 0,
                req: Ivar::new(),
                resp: Ivar::new(),
                protocol: Default::default(),
            },
        );
        sched.priorities.insert(waiter, DEFAULT_PRIORITY);
        sched.sleep_futex_waiter(&waiter, futex, None, u32::MAX);
        waiter_response = sched.next_turns[&waiter].resp.clone();
    }
    let (chosen, request, response) = state.sched.lock().unwrap().select_test_turn().unwrap();
    let mut daemon = std::pin::pin!(crate::scheduler::finish_selected_turn(
        state.sched.clone(),
        state.global_time.clone(),
        chosen,
        request,
        response.clone(),
    ));
    assert!(futures::poll!(daemon.as_mut()).is_pending());
    assert!(
        state
            .sched
            .lock()
            .unwrap()
            .run_queue
            .tentative_pop_in_progress()
    );
    let before = state.global_time.lock().unwrap().as_nanos();
    state.report_backend_failure(failure(selected));
    assert!(
        !state
            .sched
            .lock()
            .unwrap()
            .run_queue
            .tentative_pop_in_progress()
    );
    if daemon_first {
        assert!(matches!(
            futures::poll!(daemon.as_mut()),
            Poll::Ready(Err(_))
        ));
    }
    // In the other ordering, this real consuming hook performs the clear-TID
    // admission before the daemon is allowed another poll.
    consume_thread(&state, &tool, thread).await;
    if !daemon_first {
        assert!(matches!(
            futures::poll!(daemon.as_mut()),
            Poll::Ready(Err(_))
        ));
    }
    assert!(
        response.try_read().is_none(),
        "failure must not fabricate a response"
    );
    assert!(
        waiter_response.try_read().is_none(),
        "cleanup must not grant the waiter"
    );
    let after = state.global_time.lock().unwrap().as_nanos();
    assert_eq!(after, before + LogicalTime::from_nanos(37));
    {
        let mut sched = state.sched.lock().unwrap();
        assert_eq!(sched.turn, 0);
        assert!(sched.child_tid_was_cleared(futex, leader.as_raw()));
        assert_eq!(
            sched.run_queue.tids().filter(|tid| **tid == waiter).count(),
            1
        );
        assert!(!sched.next_turns.contains_key(&leader));
        assert!(
            !sched.note_deregistration_accounted(leader),
            "one accounting owner"
        );
    }
    assert!(
        crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .is_err()
    );
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), after);
    assert!(waiter_response.try_read().is_none());
}

#[tokio::test]
async fn backend_failure_closes_selection_before_consuming_clear_tid_cleanup() {
    selected_cleanup(false).await;
    selected_cleanup(true).await;
}

#[tokio::test]
async fn backend_failure_ends_quiescence_without_another_request() {
    let (_, state, tid, _) = cancellation_test_state();
    let request = Ivar::new();
    install_test_registration(&state, tid, request.clone());
    let before = state.global_time.lock().unwrap().as_nanos();
    let last = Err(crate::scheduler::SkipTurn);
    let mut turn = std::pin::pin!(crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &last,
    ));
    assert!(futures::poll!(turn.as_mut()).is_pending());
    state.report_backend_failure(failure(tid));
    assert!(matches!(futures::poll!(turn.as_mut()), Poll::Ready(Err(_))));
    assert!(request.try_read().is_none());
    assert_eq!(state.sched.lock().unwrap().turn, 0);
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), before);
}

#[tokio::test]
async fn backend_failure_completes_unstarted_daemon_without_aborting_it() {
    let config = Config {
        sequentialize_threads: true,
        ..Config::default()
    };
    let mut state = GlobalState::initialize(&config, true);
    tokio::task::yield_now().await;
    assert!(!state.sched_handle.as_ref().unwrap().is_finished());
    state.report_backend_failure(failure(DetTid::from_raw(17)));
    tokio::time::timeout(
        Duration::from_millis(100),
        state.sched_handle.take().unwrap(),
    )
    .await
    .expect("terminal startup must complete")
    .expect("daemon must not panic or be aborted");
    assert!(state.sched.lock().unwrap().started_up.try_read().is_none());
    state.clean_up(false, &None).await;
}

#[tokio::test]
async fn backend_failure_completes_registered_daemon_without_another_request() {
    let config = Config {
        sequentialize_threads: true,
        ..Config::default()
    };
    let mut state = GlobalState::initialize(&config, true);
    let tid = DetTid::from_raw(17);
    state
        .sched
        .lock()
        .unwrap()
        .thread_tree
        .add_child(tid, tid, true);
    let request = Ivar::new();
    install_test_registration(&state, tid, request.clone());
    state.sched.lock().unwrap().started_up.put(());
    tokio::task::yield_now().await;
    assert!(!state.sched_handle.as_ref().unwrap().is_finished());
    state.report_backend_failure(failure(tid));
    tokio::time::timeout(
        Duration::from_millis(100),
        state.sched_handle.take().unwrap(),
    )
    .await
    .expect("terminal quiescence must complete")
    .expect("daemon must not panic or be aborted");
    assert!(request.try_read().is_none());
    assert_eq!(state.sched.lock().unwrap().turn, 0);
    state.clean_up(false, &None).await;
}

#[tokio::test]
async fn backend_failure_precedes_ready_request_without_grant_or_timer_processing() {
    let (_, state, tid, process) = cancellation_test_state();
    let request = Ivar::full(Ok(Resources::new(tid)));
    install_test_registration(&state, tid, request);
    let before = state.global_time.lock().unwrap().as_nanos();
    let deadline = before + LogicalTime::from_nanos(37);
    state.sched.lock().unwrap().register_alarm(
        process,
        tid,
        before,
        LogicalTime::from_nanos(37),
        LogicalTime::ZERO,
        Signal::SIGALRM,
    );
    let (selected, request, response) = state.sched.lock().unwrap().select_test_turn().unwrap();
    state.report_backend_failure(failure(tid));
    assert!(
        crate::scheduler::finish_selected_turn(
            state.sched.clone(),
            state.global_time.clone(),
            selected,
            request,
            response.clone(),
        )
        .await
        .is_err()
    );
    assert!(
        crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .is_err()
    );
    assert!(response.try_read().is_none());
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), before);
    let sched = state.sched.lock().unwrap();
    assert_eq!(sched.turn, 0);
    assert_eq!(sched.blocked.timed_waiters.next_deadline(), Some(deadline));
}

async fn cancelled_constructed_child(register: bool, terminal: bool) {
    let (config, state, parent, process) = cancellation_test_state();
    install_test_registration(&state, parent, Ivar::new());
    let tool = Detcore::new(Tid::from_raw(process.as_raw()), &config);
    let mut parent_thread = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
    parent_thread.detpid = Some(process);
    parent_thread.thread_start_entered = true;
    let flags = CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM | CloneFlags::CLONE_CHILD_CLEARTID;
    parent_thread.clone_flags = Some(flags);
    let child = DetTid::from_raw(18);
    let child_thread = tool.init_thread_state(
        Tid::from_raw(child.as_raw()),
        Some((Tid::from_raw(parent.as_raw()), &parent_thread)),
    );
    assert_eq!(
        child_thread.detpid, None,
        "exercise the real delayed initialization"
    );
    assert!(
        !child_thread.thread_start_entered,
        "child construction resets the parent's marker"
    );
    parent_thread.clone_flags = None;
    let mut guest = ExternalRegistrationGuest {
        global: &state,
        config: &config,
        thread: parent_thread,
        requests: Mutex::new(Vec::new()),
    };
    if register {
        let mut registration = std::pin::pin!(tool.register_external_child(
            &mut guest,
            Tid::from_raw(child.as_raw()),
            0x404100,
            flags,
            0,
            None,
        ));
        assert!(futures::poll!(registration.as_mut()).is_pending());
        assert!(state.sched.lock().unwrap().next_turns.contains_key(&child));
        if terminal {
            state.report_backend_failure(failure(parent));
        }
        // Drop the actual parked parent callback, as the KVM driver does on
        // terminal notification. No ParentContinue reply or child start occurs.
    } else {
        if terminal {
            state.report_backend_failure(failure(parent));
        }
    }
    let before = serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap();
    consume_thread(&state, &tool, child_thread).await;
    let sched = state.sched.lock().unwrap();
    assert_eq!(sched.backend_failed(), terminal);
    assert_eq!(sched.thread_was_registered(child), register);
    assert!(!sched.next_turns.contains_key(&child));
    assert_eq!(sched.turn, 0);
    assert!(sched.next_turns[&parent].resp.try_read().is_none());
    if !register {
        assert_eq!(
            serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap(),
            before
        );
        assert!(!state.global_time.lock().unwrap().contains_thread(child));
    } else {
        assert!(sched.thread_is_logically_killed(child));
    }
}

#[tokio::test]
async fn backend_failure_consumes_registered_child_before_thread_start() {
    cancelled_constructed_child(true, true).await;
}

#[tokio::test]
async fn backend_failure_consumes_unregistered_child_without_admission_or_clock() {
    cancelled_constructed_child(false, true).await;
}

#[tokio::test]
async fn normal_cancellation_consumes_unstarted_child_without_registration() {
    cancelled_constructed_child(false, false).await;
}

#[tokio::test]
async fn backend_failure_blocks_rpc_clock_and_child_registration_at_admission() {
    let (_, state, parent, process) = cancellation_test_state();
    install_test_registration(&state, parent, Ivar::new());
    let child = DetTid::from_raw(18);
    let before = serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap();
    state.report_backend_failure(failure(parent));
    let mut time = DetTime::new(&state.cfg);
    time.add_syscall_with_cost(37);
    let mut rpc = std::pin::pin!(state.receive_rpc(
        Tid::from_raw(parent.as_raw()),
        (
            time,
            MmId::initial(process),
            GlobalRequest::CreateChildThread(
                child,
                process,
                0,
                Some(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM),
                0,
                None,
                Some(DEFAULT_PRIORITY),
            )
        ),
    ));
    assert!(
        futures::poll!(rpc.as_mut()).is_pending(),
        "no fabricated normal reply"
    );
    assert_eq!(
        serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap(),
        before
    );
    let sched = state.sched.lock().unwrap();
    assert!(!sched.thread_was_registered(child));
    assert!(!sched.next_turns.contains_key(&child));
    assert!(sched.next_turns[&parent].req.try_read().is_none());
    assert_eq!(sched.turn, 0);
}

#[tokio::test]
async fn backend_failure_blocks_registration_that_passed_earlier_header_admission() {
    let (_, state, parent, process) = cancellation_test_state();
    install_test_registration(&state, parent, Ivar::new());
    let child = DetTid::from_raw(18);
    state.report_backend_failure(failure(parent));
    // Enter the actual second mutation boundary directly, representing an RPC
    // whose header won the mutex before failure but whose registration did not.
    let mut rpc = std::pin::pin!(state.recv_create_child_thread(
        Tid::from_raw(parent.as_raw()),
        MmId::initial(process),
        super::super::ChildRegistration {
            parent_dettid: parent,
            parent_detpid: process,
            child_dettid: child,
            child_tid_addr: 0,
            flags: Some(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM),
            exit_signal: 0,
            physical_ids: None,
            maybe_priority: Some(DEFAULT_PRIORITY),
            parent_is_kernel_blocked: false,
        },
    ));
    assert!(futures::poll!(rpc.as_mut()).is_pending());
    let sched = state.sched.lock().unwrap();
    assert!(!sched.thread_was_registered(child));
    assert!(!sched.next_turns.contains_key(&child));
    assert!(sched.next_turns[&parent].req.try_read().is_none());
}

async fn unknown_deregistration(
    sender: i32,
    owner: i32,
    header_mm: i32,
    owner_mm: i32,
    started: bool,
) {
    let (_, state, _, _) = cancellation_test_state();
    let _ = state
        .receive_rpc(
            Tid::from_raw(sender),
            (
                DetTime::new(&state.cfg),
                MmId::initial(DetPid::from_raw(header_mm)),
                GlobalRequest::DeregisterThread(ThreadDeregistration {
                    dettid: DetTid::from_raw(owner),
                    detpid: DetPid::from_raw(owner_mm),
                    mm: MmId::initial(DetPid::from_raw(owner_mm)),
                    thread_start_entered: started,
                    timeslice_stats: TimesliceStats::default(),
                    syscall_count: 0,
                    chaos_epochs: Vec::new(),
                }),
            ),
        )
        .await;
}

#[tokio::test]
#[should_panic(expected = "a started thread must have a scheduler registration")]
async fn ordinary_unknown_started_owner_is_not_acknowledged_as_unstarted() {
    unknown_deregistration(19, 19, 17, 17, true).await;
}

#[tokio::test]
async fn dbt_missing_physical_id_start_preserves_tombstone_deregistration_accounting() {
    let config = Config {
        sequentialize_threads: true,
        cancel_killed_thread_rpcs: true,
        backend_requires_thread_directed_process_signals: true,
        ..Config::default()
    };
    let state = GlobalState::initialize(&config, false);
    let tid = DetTid::from_raw(19);
    let process = DetPid::from_raw(17);
    let mm = MmId::initial(process);
    // The parent process is already running while its new child's Start RPC
    // races the parent's CreateChildThread. Only the child is unregistered.
    state
        .sched
        .lock()
        .unwrap()
        .thread_tree
        .add_child(process, process, true);
    install_test_registration(&state, process, Ivar::new());
    {
        let sched = state.sched.lock().unwrap();
        assert!(sched.thread_was_registered(process));
        assert!(!sched.thread_was_registered(tid));
    }
    // Exercise the real DBT defensive refusal before CreateChildThread. Do
    // not install the child registration or fabricate its logical tombstone.
    let response = tokio::time::timeout(
        Duration::from_millis(100),
        state.receive_rpc(
            Tid::from_raw(tid.as_raw()),
            (
                DetTime::new(&config),
                mm,
                GlobalRequest::StartNewThread(tid, process, None, None),
            ),
        ),
    )
    .await
    .expect("a missing physical ID must not wait for parent registration");
    assert_eq!(response, (None, GlobalResponse::ThreadExited));
    {
        let sched = state.sched.lock().unwrap();
        assert!(!sched.backend_failed());
        assert!(!sched.thread_was_registered(tid));
        assert!(sched.thread_is_logically_killed(tid));
    }
    let before = serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap();
    let mut stats = TimesliceStats::default();
    stats.record(7);
    // Distinct final and duplicate payloads prove that the consuming RPC
    // reaches existing accounting exactly once, rather than an early reply.
    for count in [17, 99] {
        let response = state
            .receive_rpc(
                Tid::from_raw(tid.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        dettid: tid,
                        detpid: process,
                        mm,
                        thread_start_entered: true,
                        timeslice_stats: stats,
                        syscall_count: count,
                        chaos_epochs: Vec::new(),
                    }),
                ),
            )
            .await;
        assert_eq!(response, (None, GlobalResponse::DeregisterThread(())));
        let mut sched = state.sched.lock().unwrap();
        assert!(!sched.note_deregistration_accounted(tid));
        assert_eq!(sched.per_thread_timeslice.get(&tid), Some(&stats));
        assert_eq!(sched.per_thread_syscalls.get(&tid), Some(&17));
        assert!(!sched.next_turns.contains_key(&tid));
        assert_eq!(sched.turn, 0);
    }
    assert_eq!(
        serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap(),
        before
    );
}

#[tokio::test]
async fn backend_failure_cleanup_does_not_build_an_invalid_run_summary() {
    let config = Config {
        sequentialize_threads: true,
        ..Config::default()
    };
    let future = SystemTime::now() + Duration::from_secs(60);
    let mut premise = GlobalState::initialize(&config, false);
    premise.realtime_start = future;
    assert!(premise.into_run_summary().is_err());
    let directory = tempfile::tempdir().unwrap();
    let recording = directory.path().join("partial-preemptions.json");
    let config = Config {
        record_preemptions: true,
        record_preemptions_to: Some(recording.clone()),
        ..config
    };
    let mut state = GlobalState::initialize(&config, true);
    state.realtime_start = future;
    let tid = DetTid::from_raw(17);
    let time = LogicalTime::from_nanos(37);
    {
        let mut sched = state.sched.lock().unwrap();
        let writer = sched.preemption_writer.as_mut().unwrap();
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_reprioritization(tid, time, 23, DEFAULT_PRIORITY, DEFAULT_PRIORITY + 1);
    }
    assert!(!recording.exists());
    state.report_backend_failure(failure(tid));
    let cleanup = tokio::time::timeout(
        Duration::from_millis(100),
        state.clean_up_after_backend_failure(),
    )
    .await
    .expect("the failed daemon must finish naturally");
    assert!(cleanup.scheduler.is_ok());
    assert!(cleanup.preemption_recording.is_ok());
    let actual: crate::preemptions::PreemptionRecord =
        serde_json::from_slice(&std::fs::read(recording).unwrap()).unwrap();
    actual.validate().unwrap();
    let mut expected = crate::preemptions::ThreadHistory::new()
        .with_prio_changes(vec![(time, DEFAULT_PRIORITY)])
        .with_preemption_rcbs(vec![23]);
    expected.final_prio = DEFAULT_PRIORITY + 1;
    assert_eq!(
        actual.extract_all(),
        std::collections::BTreeMap::from([(tid, expected)])
    );
    assert!(actual.schedevents().is_empty());
}

#[tokio::test]
async fn backend_failure_cleanup_retains_panicked_scheduler_and_recording_error() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        sequentialize_threads: true,
        record_preemptions: true,
        record_preemptions_to: Some(directory.path().to_path_buf()),
        ..Config::default()
    };
    let mut state = GlobalState::initialize(&config, false);
    let sched = state.sched.clone();
    state.sched_handle = Some(tokio::spawn(async move {
        let _lock = sched.lock().unwrap();
        panic!("scheduler cleanup control");
    }));
    let cleanup = tokio::time::timeout(
        Duration::from_millis(100),
        state.clean_up_after_backend_failure(),
    )
    .await
    .expect("the owned task must finish without an abort");
    assert!(cleanup.scheduler.unwrap_err().is_panic());
    assert!(cleanup.preemption_recording.is_err());
    assert!(directory.path().is_dir());
}

#[tokio::test]
#[should_panic(expected = "deregistration must belong to its sender")]
async fn unstarted_cleanup_rejects_another_senders_identity() {
    unknown_deregistration(19, 23, 17, 17, false).await;
}

#[tokio::test]
#[should_panic(expected = "deregistration must retain its MmId")]
async fn unstarted_cleanup_rejects_a_different_payload_mm() {
    unknown_deregistration(19, 19, 17, 23, false).await;
}

#[tokio::test]
async fn thread_start_marker_is_set_before_the_first_registration_wait() {
    let config = Config {
        sequentialize_threads: true,
        ..Config::default()
    };
    let state = GlobalState::initialize(&config, false);
    let tid = Tid::from_raw(17);
    let tool = Detcore::new(tid, &config);
    let thread = tool.init_thread_state(tid, None);
    assert!(!thread.thread_start_entered);
    let mut guest = ExternalRegistrationGuest {
        global: &state,
        config: &config,
        thread,
        requests: Mutex::new(Vec::new()),
    };
    // This test Guest's pid accessor expects the normal process identity.
    guest.thread.detpid = Some(DetPid::from_raw(17));
    {
        let mut start = std::pin::pin!(tool.handle_thread_start(&mut guest));
        assert!(futures::poll!(start.as_mut()).is_pending());
    }
    assert!(guest.thread.thread_start_entered);
    assert!(
        state
            .sched
            .lock()
            .unwrap()
            .thread_was_registered(DetTid::from_raw(17))
    );
    state.report_backend_failure(failure(DetTid::from_raw(17)));
    consume_thread(&state, &tool, guest.thread).await;
}

#[test]
fn both_fork_and_thread_construction_reset_the_parent_start_marker() {
    let config = Config::default();
    let parent = Tid::from_raw(17);
    let tool: Detcore = Detcore::new(parent, &config);
    let mut state = tool.init_thread_state(parent, None);
    state.thread_start_entered = true;
    for flags in [
        CloneFlags::empty(),
        CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM,
    ] {
        state.clone_flags = Some(flags);
        let child = tool.init_thread_state(Tid::from_raw(19), Some((parent, &state)));
        assert!(!child.thread_start_entered);
        assert!(state.thread_start_entered);
    }
}
