/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Poll;

use reverie::Errno;
use reverie::ExitStatus;
use reverie::Tool;

use super::*;
use crate::ThreadState;
use crate::types::ChildWaitExitClass;
use crate::types::ChildWaitSelector;
use crate::types::ChildWaitSpec;
use crate::types::ExactChildWaitState;

const PARENT: DetTid = DetTid::from_raw(1);
const LEADER: DetTid = DetTid::from_raw(17);
const WORKER: DetTid = DetTid::from_raw(18);

#[derive(Clone, Copy, PartialEq)]
enum Gate {
    None,
    BeforeCommit,
    AfterCommit,
}

// This transport binds the backend's sender independently of ThreadState.
// Both gated paths execute the production RPC; no successful reply is invented.
struct ExecRpc<'a> {
    state: &'a GlobalState,
    sender: DetTid,
    gate: Gate,
    reached: AtomicBool,
    release: Ivar<()>,
    requests: Mutex<Vec<GlobalRequest>>,
}

impl<'a> ExecRpc<'a> {
    fn new(state: &'a GlobalState, sender: DetTid, gate: Gate) -> Self {
        Self {
            state,
            sender,
            gate,
            reached: AtomicBool::new(false),
            release: Ivar::new(),
            requests: Mutex::new(Vec::new()),
        }
    }

    async fn suspend(&self) {
        self.reached.store(true, Ordering::SeqCst);
        self.release.clone().await;
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for ExecRpc<'_> {
    async fn send_rpc(
        &self,
        request: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let reconnect = matches!(&request.2, GlobalRequest::ReconnectExec { .. });
        self.requests.lock().unwrap().push(request.2.clone());
        if reconnect && self.gate == Gate::BeforeCommit {
            self.suspend().await;
        }
        let result = self
            .state
            .receive_rpc(Tid::from_raw(self.sender.as_raw()), request)
            .await;
        if reconnect && self.gate == Gate::AfterCommit {
            assert_eq!(result, (None, GlobalResponse::ReconnectExec(true)));
            self.suspend().await;
        }
        result
    }

    fn config(&self) -> &Config {
        &self.state.cfg
    }
}

struct ExecGuest<'a> {
    rpc: ExecRpc<'a>,
    thread: ThreadState<()>,
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for ExecGuest<'_> {
    async fn send_rpc(
        &self,
        request: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.rpc.send_rpc(request).await
    }

    fn config(&self) -> &Config {
        self.rpc.config()
    }
}

#[reverie::tool]
impl Guest<Detcore> for ExecGuest<'_> {
    type Memory = reverie::syscalls::LocalMemory;
    type Stack = ExternalRegistrationStack;

    fn tid(&self) -> Tid {
        Tid::from_raw(self.rpc.sender.as_raw())
    }
    fn pid(&self) -> Tid {
        Tid::from_raw(LEADER.as_raw())
    }
    fn ppid(&self) -> Option<Tid> {
        Some(Tid::from_raw(PARENT.as_raw()))
    }
    fn memory(&self) -> Self::Memory {
        panic!("exec identity handoff must not access guest memory")
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("exec identity handoff must not read registers")
    }
    async fn stack(&mut self) -> Self::Stack {
        panic!("exec identity handoff must not use a guest stack")
    }
    async fn daemonize(&mut self) {
        panic!("exec identity handoff must not daemonize")
    }
    async fn inject<S: reverie::syscalls::SyscallInfo>(
        &mut self,
        _: S,
    ) -> Result<i64, reverie::Errno> {
        panic!("exec identity handoff must not inject syscalls")
    }
    async fn tail_inject<S: reverie::syscalls::SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("exec identity handoff must not retire a live guest")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("identity transfer must preserve the existing PMU timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("identity transfer must preserve the existing PMU timer")
    }
    fn read_clock(&mut self) -> Result<u64, reverie::Error> {
        panic!("identity transfer must preserve the carried clock")
    }
}

struct Fixture {
    state: GlobalState,
    tool: Detcore,
    parent: ThreadState<()>,
    worker: ThreadState<()>,
    before_tail: LogicalTime,
}

fn child_wait() -> ChildWaitSpec {
    ChildWaitSpec {
        selector: ChildWaitSelector::Exact(LEADER),
        owner: None,
        exit_class: ChildWaitExitClass::Sigchld,
    }
}

async fn consume(state: &GlobalState, tool: &Detcore, sender: DetTid, thread: ThreadState<()>) {
    consume_with_status(state, tool, sender, thread, ExitStatus::Exited(73)).await;
}

async fn consume_with_status(
    state: &GlobalState,
    tool: &Detcore,
    sender: DetTid,
    thread: ThreadState<()>,
    status: ExitStatus,
) {
    let rpc = ExecRpc::new(state, sender, Gate::None);
    let mut exit =
        Box::pin(tool.on_exit_thread(Tid::from_raw(sender.as_raw()), &rpc, thread, status));
    assert!(
        matches!(futures::poll!(&mut exit), Poll::Ready(Ok(()))),
        "consuming cleanup must finish without a scheduler grant"
    );
    assert_eq!(rpc.requests.lock().unwrap().len(), 1);
}

impl Fixture {
    async fn prepared() -> Self {
        Self::with_leader_retired(true).await
    }

    async fn with_leader_retired(retire_leader: bool) -> Self {
        Self::configured(retire_leader, false).await
    }

    async fn configured(retire_leader: bool, backend_serializes_fork_children: bool) -> Self {
        let config = Config {
            sequentialize_threads: true,
            cancel_killed_thread_rpcs: false,
            backend_serializes_fork_children,
            max_timeslice: std::num::NonZeroU64::new(200_000_000),
            ..Config::default()
        };
        assert!(config.use_rcb_time());
        let state = GlobalState::initialize(&config, false);
        let tool = Detcore::new(Tid::from_raw(LEADER.as_raw()), &config);
        let mut parent = tool.init_thread_state(Tid::from_raw(PARENT.as_raw()), None);
        parent.detpid = Some(PARENT);
        parent.thread_start_entered = true;
        parent.thread_logical_time.add_syscall_with_cost(1_000);
        parent.thread_logical_time.add_rcbs(11);
        parent.account_process_cpu_time();
        parent.clone_flags = Some(CloneFlags::empty());
        let mut leader = tool.init_thread_state(
            Tid::from_raw(LEADER.as_raw()),
            Some((Tid::from_raw(PARENT.as_raw()), &parent)),
        );
        leader.detpid = Some(LEADER);
        leader.thread_start_entered = true;
        leader.thread_logical_time.add_syscall_with_cost(101);
        leader.thread_logical_time.add_rcbs(13);
        leader.account_process_cpu_time();
        leader.stats.syscall_count = 3;
        leader.stats.timeslice_stats.record(23);
        leader.clone_flags = Some(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM);
        let mut worker = tool.init_thread_state(
            Tid::from_raw(WORKER.as_raw()),
            Some((Tid::from_raw(LEADER.as_raw()), &leader)),
        );
        worker.detpid = Some(LEADER);
        worker.thread_start_entered = true;
        worker.thread_logical_time.add_syscall_with_cost(211);
        worker.thread_logical_time.add_rcbs(17);
        worker.account_process_cpu_time();
        worker.stats.syscall_count = 7;
        worker.stats.timeslice_stats.record(41);
        let old_mm = worker.mm_id;
        {
            let mut sched = state.sched.lock().unwrap();
            sched.thread_tree.add_child(PARENT, PARENT, true);
            sched.thread_tree.add_child(PARENT, LEADER, true);
            sched.thread_tree.add_child(LEADER, WORKER, false);
        }
        for thread in [&leader, &worker] {
            install_test_registration(&state, thread.dettid, Ivar::new());
            state
                .sched
                .lock()
                .unwrap()
                .install_test_exec_incarnation(thread.dettid, old_mm);
        }
        for thread in [&parent, &leader, &worker] {
            state.global_time.lock().unwrap().update_global_time(
                thread.dettid,
                thread.thread_logical_time.as_nanos(),
                thread.thread_logical_time.inherited_nanos(),
            );
        }
        let prepared = state
            .receive_rpc(
                Tid::from_raw(WORKER.as_raw()),
                (
                    worker.thread_logical_time.clone(),
                    old_mm,
                    GlobalRequest::PrepareExec(LEADER, old_mm, Default::default()),
                ),
            )
            .await;
        assert_eq!(prepared, (None, GlobalResponse::PrepareExec(())));
        let before_tail = state.global_time.lock().unwrap().as_nanos();
        let epoch = DetTime::new(&config).as_nanos();
        assert_eq!(before_tail, epoch + LogicalTime::from_nanos(1_722));
        worker.thread_logical_time.add_syscall_with_cost(307);
        worker.thread_logical_time.add_rcbs(19);
        worker.mm_id = old_mm.for_exec(LEADER);
        // Ptrace consumes the displaced leader before delivering successful
        // exec to the surviving worker, whose backend TID is now LEADER.
        if retire_leader {
            consume(&state, &tool, LEADER, leader).await;
        }
        let fixture = Self {
            state,
            tool,
            parent,
            worker,
            before_tail,
        };
        fixture.assert_displaced_leader_receipt(retire_leader);
        fixture.assert_running();
        fixture
    }

    fn assert_running(&self) {
        let mut sched = self.state.sched.lock().unwrap();
        assert_eq!(
            sched.exact_child_wait_state(PARENT, LEADER),
            ExactChildWaitState::Running
        );
        assert_eq!(sched.ready_child_wait(PARENT, child_wait()), None);
        assert_eq!(sched.turn, 0, "identity transfer cannot grant a guest turn");
    }

    fn assert_displaced_leader_receipt(&self, observed: bool) {
        let sched = self.state.sched.lock().unwrap();
        let old_mm = MmId::initial(LEADER);
        // Physical consumption is authenticated separately from canonical
        // scheduler retirement. Its original registration remains in place
        // until the complete exec teardown can retire siblings in fixed order.
        assert!(sched.next_turns.contains_key(&LEADER));
        assert!(sched.next_turns[&LEADER].req.try_read().is_none());
        assert!(sched.rpc_incarnation_matches(LEADER, old_mm));
        assert_eq!(
            sched.exec_sibling_retirement_observed(LEADER, LEADER, old_mm),
            observed
        );
        assert!(!sched.exec_sibling_retirement_observed(LEADER, PARENT, old_mm));
        assert!(!sched.exec_sibling_retirement_observed(LEADER, LEADER, old_mm.for_exec(LEADER)));
        assert_eq!(
            sched.per_thread_syscalls.get(&LEADER),
            if observed { Some(&3) } else { None }
        );
    }

    fn guest(&self, gate: Gate) -> ExecGuest<'_> {
        ExecGuest {
            rpc: ExecRpc::new(&self.state, LEADER, gate),
            thread: self.worker.clone(),
        }
    }

    fn assert_final_clock(&self, owner: DetTid) {
        let global = self.state.global_time.lock().unwrap();
        assert_eq!(
            global.as_nanos(),
            self.before_tail + LogicalTime::from_nanos(497)
        );
        assert_eq!(
            global.threads_time(owner),
            self.worker.thread_logical_time.as_nanos()
        );
        let snapshot = serde_json::to_value(&*global).unwrap();
        assert_eq!(
            snapshot["inherited_time"][owner.as_raw().to_string()],
            serde_json::to_value(self.worker.thread_logical_time.inherited_nanos()).unwrap()
        );
    }

    fn assert_final_cpu_and_reap(&mut self) {
        let parent = serde_json::to_value(&*self.parent.process_cpu_time.lock().unwrap()).unwrap();
        let child = &parent["exited_children"][LEADER.as_raw().to_string()];
        assert_eq!(child["user"], serde_json::json!(490));
        assert_eq!(child["system"], serde_json::json!(619));
        for _ in 0..2 {
            self.parent.reap_child_process_cpu_time(LEADER);
            let cpu = self.parent.process_cpu_time();
            assert_eq!(cpu.children_user, LogicalTime::from_nanos(490));
            assert_eq!(cpu.children_system, LogicalTime::from_nanos(619));
        }
    }

    fn assert_final_stats(&self, committed: bool) {
        let sched = self.state.sched.lock().unwrap();
        let owner = if committed { LEADER } else { WORKER };
        let owner_count = if committed { 10 } else { 7 };
        assert_eq!(sched.per_thread_syscalls.get(&owner), Some(&owner_count));
        assert_eq!(sched.per_thread_syscalls.values().sum::<u64>(), 10);
        let mut expected = TimesliceStats::default();
        expected.record(41);
        if committed {
            expected.record(23);
        }
        assert_eq!(sched.per_thread_timeslice.get(&owner), Some(&expected));
        assert_eq!(
            sched
                .per_thread_timeslice
                .values()
                .map(|s| s.count)
                .sum::<u64>(),
            2
        );
        assert_eq!(
            sched
                .per_thread_timeslice
                .values()
                .map(|s| s.sum_ns)
                .sum::<u64>(),
            64
        );
    }

    async fn duplicate_bound_exit(&self) {
        let mut duplicate = deregistration(&self.worker);
        duplicate.dettid = LEADER;
        duplicate.syscall_count = 999;
        duplicate.timeslice_stats.record(999);
        let response = self
            .state
            .receive_rpc(
                Tid::from_raw(LEADER.as_raw()),
                (
                    self.worker.thread_logical_time.clone(),
                    duplicate.mm,
                    GlobalRequest::DeregisterThread(duplicate),
                ),
            )
            .await;
        assert_eq!(response, (None, GlobalResponse::DeregisterThread(())));
        self.assert_final_stats(true);
        self.assert_final_clock(LEADER);
    }
}

fn deregistration(thread: &ThreadState<()>) -> ThreadDeregistration {
    ThreadDeregistration {
        dettid: thread.dettid,
        detpid: thread.detpid.unwrap(),
        mm: thread.mm_id,
        thread_start_entered: thread.thread_start_entered,
        timeslice_stats: thread.stats.timeslice_stats,
        syscall_count: thread.stats.syscall_count,
        chaos_epochs: Vec::new(),
    }
}

#[tokio::test]
async fn transferred_exec_preemption_artifacts_refuse_before_replacement_turn() {
    for mode in 0..3 {
        let mut fixture = Fixture::prepared().await;
        // Exercise each public configuration spelling independently. The
        // artifact reader/writer's real file lifecycle is covered by the CLI
        // record-then-replay control; here the actual reconnect RPC must keep
        // its identity and clock invariants without granting replacement work.
        match mode {
            0 => fixture.state.cfg.record_preemptions = true,
            1 => fixture.state.cfg.record_preemptions_to = Some("record.json".into()),
            2 => fixture.state.cfg.replay_preemptions_from = Some("record.json".into()),
            _ => unreachable!(),
        }
        let mut guest = fixture.guest(Gate::None);
        let clock = guest.thread.thread_logical_time.clone();
        let deadline = clock.as_nanos() + LogicalTime::from_nanos(123_456);
        guest.thread.last_rcb_timer = Some(98_765);
        guest.thread.end_of_timeslice = Some(deadline);
        guest.thread.max_timeslice_end = Some(deadline);
        let mut reconnect = Box::pin(super::super::reconnect_exec(&mut guest));
        assert_eq!(
            futures::poll!(&mut reconnect),
            Poll::Ready(Err(Errno::EOPNOTSUPP))
        );
        drop(reconnect);
        assert_eq!(guest.thread.dettid, LEADER);
        assert_eq!(
            guest.thread.thread_logical_time.as_nanos(),
            clock.as_nanos()
        );
        assert_eq!(guest.thread.last_rcb_timer, Some(98_765));
        assert_eq!(guest.thread.end_of_timeslice, Some(deadline));
        assert_eq!(guest.thread.max_timeslice_end, Some(deadline));
        assert_eq!(
            *guest.rpc.requests.lock().unwrap(),
            [GlobalRequest::ReconnectExec {
                former: WORKER,
                process: LEADER,
            }],
            "refusal must precede ResumeExec or any replacement-image RPC"
        );
        fixture.assert_running();
        fixture.assert_final_clock(LEADER);
        let sched = fixture.state.sched.lock().unwrap();
        assert!(!sched.next_turns.contains_key(&WORKER));
        assert!(sched.next_turns[&LEADER].req.try_read().is_none());
        assert!(sched.next_turns[&LEADER].resp.try_read().is_none());
        assert!(fixture.state.pending_exec_states.lock().unwrap().is_empty());
        assert!(
            fixture
                .state
                .completed_exec_transfers
                .lock()
                .unwrap()
                .contains_key(&LEADER)
        );
    }
}

#[tokio::test]
async fn preemption_artifact_refusal_is_specific_to_transferred_exec() {
    let mut fixture = Fixture::prepared().await;
    fixture.state.cfg.record_preemptions = true;
    fixture.state.cfg.record_preemptions_to = Some("record.json".into());
    fixture.state.cfg.replay_preemptions_from = Some("record.json".into());
    let mut same_tid = ExecGuest {
        rpc: ExecRpc::new(&fixture.state, WORKER, Gate::None),
        thread: fixture.worker.clone(),
    };
    assert_eq!(super::super::reconnect_exec(&mut same_tid).await, Ok(()));
    assert!(same_tid.rpc.requests.lock().unwrap().is_empty());
    fixture.assert_displaced_leader_receipt(true);

    // Ordinary chaos execution without artifact flags still reaches its real
    // continuation request. Do not globally disable preemption or exec.
    let mut ordinary = Fixture::prepared().await;
    ordinary.state.cfg.chaos = true;
    let mut guest = ordinary.guest(Gate::None);
    {
        let mut reconnect = Box::pin(super::super::reconnect_exec(&mut guest));
        assert!(futures::poll!(&mut reconnect).is_pending());
    }
    assert_eq!(guest.thread.dettid, LEADER);
    assert!(matches!(
        guest.rpc.requests.lock().unwrap().as_slice(),
        [
            GlobalRequest::ReconnectExec { .. },
            GlobalRequest::ResumeExec(LEADER)
        ]
    ));
}

async fn cancelled_transfer(gate: Gate, backend_failed: bool) {
    let mut fixture = Fixture::prepared().await;
    let mut guest = fixture.guest(gate);
    {
        let mut reconnect = Box::pin(super::super::reconnect_exec(&mut guest));
        assert!(futures::poll!(&mut reconnect).is_pending());
        // Drop the real callback at the transport boundary, as backend cleanup
        // does. The local carried identity has not received an acknowledgment.
    }
    assert!(guest.rpc.reached.load(Ordering::SeqCst));
    assert_eq!(guest.thread.dettid, WORKER);
    assert_eq!(guest.rpc.requests.lock().unwrap().len(), 1);
    fixture.assert_running();
    let committed = gate == Gate::AfterCommit;
    assert_eq!(
        fixture
            .state
            .completed_exec_transfers
            .lock()
            .unwrap()
            .contains_key(&LEADER),
        committed
    );
    assert_eq!(
        fixture
            .state
            .pending_exec_states
            .lock()
            .unwrap()
            .contains_key(&LEADER),
        !committed
    );
    if committed {
        fixture.assert_final_clock(LEADER);
        assert!(
            !fixture
                .state
                .global_time
                .lock()
                .unwrap()
                .contains_thread(WORKER)
        );
        assert!(
            fixture.state.sched.lock().unwrap().next_turns[&LEADER]
                .req
                .try_read()
                .is_none()
        );
    } else {
        assert_eq!(
            fixture.state.global_time.lock().unwrap().as_nanos(),
            fixture.before_tail
        );
        fixture.assert_displaced_leader_receipt(true);
    }
    let duplicate = deregistration(&guest.thread);
    if backend_failed {
        fixture
            .state
            .report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(LEADER.as_raw()),
                tid: Tid::from_raw(LEADER.as_raw()),
                phase: "exec transport cancellation control",
            });
    }
    let status = if backend_failed {
        // A failure can race a task's actual normal exit. The published global
        // failure, rather than this numeric status, authorizes cleanup.
        ExitStatus::Exited(73)
    } else {
        ExitStatus::Signaled(Signal::SIGKILL, false)
    };
    consume_with_status(&fixture.state, &fixture.tool, LEADER, guest.thread, status).await;
    let owner = if committed { LEADER } else { WORKER };
    fixture.assert_final_clock(owner);
    assert!(fixture.state.pending_exec_states.lock().unwrap().is_empty());
    assert!(
        fixture
            .state
            .completed_exec_transfers
            .lock()
            .unwrap()
            .is_empty()
    );
    {
        let mut sched = fixture.state.sched.lock().unwrap();
        assert!(!sched.next_turns.contains_key(&LEADER));
        assert!(!sched.next_turns.contains_key(&WORKER));
        assert_eq!(sched.backend_failed(), backend_failed);
        assert_eq!(sched.turn, 0);
        assert_eq!(
            sched.exact_child_wait_state(PARENT, LEADER),
            ExactChildWaitState::LogicallyExited
        );
        assert_eq!(sched.ready_child_wait(PARENT, child_wait()), Some(LEADER));
    }
    fixture.assert_final_stats(committed);
    let before = serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap();
    let mut forged_time = fixture.worker.thread_logical_time.clone();
    forged_time.add_syscall_with_cost(999_999);
    let mut duplicate = duplicate;
    duplicate.syscall_count = 999;
    let replayed = fixture
        .state
        .receive_rpc(
            Tid::from_raw(LEADER.as_raw()),
            (
                forged_time,
                duplicate.mm,
                GlobalRequest::RetireExec {
                    thread: duplicate,
                    signaled: true,
                },
            ),
        )
        .await;
    assert_eq!(replayed, (None, GlobalResponse::RetireExec(false)));
    assert_eq!(
        serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
        before
    );
    fixture.assert_final_stats(committed);
    if committed {
        fixture.duplicate_bound_exit().await;
    }
    fixture.assert_final_cpu_and_reap();
}

#[tokio::test]
async fn cancelled_before_exec_commit_consumes_original_owner_without_admission() {
    cancelled_transfer(Gate::BeforeCommit, false).await;
}

#[tokio::test]
async fn cancelled_after_exec_commit_before_reply_consumes_replacement_once() {
    cancelled_transfer(Gate::AfterCommit, false).await;
}

#[tokio::test]
async fn backend_failure_before_exec_commit_still_consumes_original_owner() {
    cancelled_transfer(Gate::BeforeCommit, true).await;
}

#[tokio::test]
async fn backend_failure_after_exec_commit_still_consumes_replacement() {
    cancelled_transfer(Gate::AfterCommit, true).await;
}

#[tokio::test]
async fn unbound_exec_ordinary_rpc_fails_before_sending_or_retiring() {
    let fixture = Fixture::prepared().await;
    let mut guest = fixture.guest(Gate::None);
    let before = serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap();
    // This is the first ordinary helper reached if handle_post_exec omits its
    // reconnect. Catching just any panic would also accept the mock's forbidden
    // tail-injection panic, so require the protocol diagnostic AND no RPC.
    let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
        super::super::send_and_update_time(&mut guest, GlobalRequest::GlobalTimeLowerBound),
    ))
    .await;
    let panic = result.expect_err("an unbound survivor must not request ordinary work");
    let diagnostic = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .expect("identity assertion must provide a diagnostic");
    assert!(diagnostic.contains("replacement image must reconnect before ordinary RPCs"));
    assert!(guest.rpc.requests.lock().unwrap().is_empty());
    assert_eq!(guest.thread.dettid, WORKER);
    assert_eq!(
        serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
        before
    );
    assert!(
        fixture
            .state
            .pending_exec_states
            .lock()
            .unwrap()
            .contains_key(&LEADER)
    );
    assert!(
        fixture
            .state
            .completed_exec_transfers
            .lock()
            .unwrap()
            .is_empty()
    );
    fixture.assert_running();
}

#[tokio::test]
async fn unbound_exec_normal_exit_cannot_consume_pending_or_completed_transfer() {
    for committed in [false, true] {
        for status in [0, 73] {
            let fixture = Fixture::prepared().await;
            if committed {
                commit_without_acknowledgment(&fixture).await;
            }
            let before = serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap();
            let (syscalls, timeslices) = {
                let sched = fixture.state.sched.lock().unwrap();
                (
                    sched.per_thread_syscalls.clone(),
                    sched.per_thread_timeslice.clone(),
                )
            };
            let rpc = ExecRpc::new(&fixture.state, LEADER, Gate::None);
            let mut exit = Box::pin(fixture.tool.on_exit_thread(
                Tid::from_raw(LEADER.as_raw()),
                &rpc,
                fixture.worker.clone(),
                ExitStatus::Exited(status),
            ));
            let error = match futures::poll!(&mut exit) {
                Poll::Ready(Err(error)) => error,
                result => panic!("normal exit {status} must reject unbound cleanup: {result:?}"),
            };
            assert!(error.to_string().contains("EINVAL"));
            let requests = rpc.requests.lock().unwrap();
            assert!(matches!(
                requests.as_slice(),
                [GlobalRequest::RetireExec {
                    signaled: false,
                    ..
                }]
            ));
            assert_eq!(
                serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
                before
            );
            {
                let sched = fixture.state.sched.lock().unwrap();
                assert!(!sched.backend_failed());
                assert_eq!(sched.per_thread_syscalls, syscalls);
                assert_eq!(sched.per_thread_timeslice, timeslices);
                assert!(sched.next_turns.contains_key(&LEADER));
                assert_eq!(sched.next_turns.contains_key(&WORKER), !committed);
                assert_eq!(
                    sched.exec_sibling_retirement_observed(LEADER, LEADER, MmId::initial(LEADER)),
                    !committed
                );
            }
            assert_eq!(
                fixture
                    .state
                    .pending_exec_states
                    .lock()
                    .unwrap()
                    .contains_key(&LEADER),
                !committed
            );
            assert_eq!(
                fixture
                    .state
                    .completed_exec_transfers
                    .lock()
                    .unwrap()
                    .contains_key(&LEADER),
                committed
            );
            fixture.assert_running();
        }
    }
}

#[tokio::test]
async fn transferred_exec_waits_for_real_grant_and_preserves_clock_and_deadlines() {
    let mut fixture = Fixture::prepared().await;
    let mut guest = fixture.guest(Gate::None);
    let deadline = guest.thread.thread_logical_time.as_nanos() + LogicalTime::from_nanos(12_345);
    guest.thread.end_of_timeslice = Some(deadline);
    guest.thread.max_timeslice_end = Some(deadline);
    guest.thread.last_rcb_timer = Some(1_234);
    let clock = serde_json::to_value(&guest.thread.thread_logical_time).unwrap();
    {
        let mut reconnect = Box::pin(super::super::reconnect_exec(&mut guest));
        assert!(futures::poll!(&mut reconnect).is_pending());
        assert!(futures::poll!(&mut reconnect).is_pending());
        fixture.assert_running();
        fixture.assert_final_clock(LEADER);
        let turn = crate::scheduler::do_a_turn_blocking(
            fixture.state.sched.clone(),
            fixture.state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .expect("replacement must receive its continuation grant");
        assert_eq!(turn.tid, LEADER);
        assert_eq!(reconnect.await, Ok(()));
    }
    assert_eq!(guest.thread.dettid, LEADER);
    assert_eq!(
        serde_json::to_value(&guest.thread.thread_logical_time).unwrap(),
        clock
    );
    assert_eq!(guest.thread.end_of_timeslice, Some(deadline));
    assert_eq!(guest.thread.max_timeslice_end, Some(deadline));
    assert_eq!(guest.thread.last_rcb_timer, Some(1_234));
    assert_eq!(
        *guest.rpc.requests.lock().unwrap(),
        vec![
            GlobalRequest::ReconnectExec {
                former: WORKER,
                process: LEADER
            },
            GlobalRequest::ResumeExec(LEADER),
        ]
    );
    assert_eq!(fixture.state.sched.lock().unwrap().turn, 1);
    assert!(
        fixture
            .state
            .completed_exec_transfers
            .lock()
            .unwrap()
            .is_empty()
    );
    // The same ordinary helper is legal as soon as acknowledgment has bound
    // the carried state to the backend's current identity.
    assert_eq!(
        super::super::send_and_update_time(&mut guest, GlobalRequest::GlobalTimeLowerBound).await,
        (
            None,
            GlobalResponse::GlobalTimeLowerBound(
                fixture.before_tail + LogicalTime::from_nanos(497)
            )
        )
    );
    consume(&fixture.state, &fixture.tool, LEADER, guest.thread).await;
    fixture.assert_final_clock(LEADER);
    fixture.assert_final_stats(true);
    fixture.duplicate_bound_exit().await;
    fixture.assert_final_cpu_and_reap();
}

#[tokio::test]
async fn malformed_exec_transfer_never_accounts_clock_or_consumes_preparation() {
    let fixture = Fixture::prepared().await;
    let before = serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap();
    let old_mm = MmId::initial(LEADER);
    let new_mm = fixture.worker.mm_id;
    for (sender, former, process, mm) in [
        (WORKER, WORKER, LEADER, new_mm),
        (LEADER, LEADER, LEADER, new_mm),
        (LEADER, PARENT, LEADER, new_mm),
        (LEADER, WORKER, PARENT, new_mm),
        (LEADER, WORKER, LEADER, old_mm),
        (LEADER, WORKER, LEADER, new_mm.for_exec(LEADER)),
    ] {
        let mut time = fixture.worker.thread_logical_time.clone();
        time.add_syscall_with_cost(999_999);
        let result = fixture
            .state
            .receive_rpc(
                Tid::from_raw(sender.as_raw()),
                (time, mm, GlobalRequest::ReconnectExec { former, process }),
            )
            .await;
        assert_eq!(result, (None, GlobalResponse::ReconnectExec(false)));
        assert_eq!(
            serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
            before
        );
        assert!(
            fixture
                .state
                .pending_exec_states
                .lock()
                .unwrap()
                .contains_key(&LEADER)
        );
        assert!(
            fixture
                .state
                .completed_exec_transfers
                .lock()
                .unwrap()
                .is_empty()
        );
        fixture.assert_running();
    }
}

#[tokio::test]
async fn failed_exec_cancel_removes_authority_for_later_identity_takeover() {
    let fixture = Fixture::prepared().await;
    let result = fixture
        .state
        .receive_rpc(
            Tid::from_raw(WORKER.as_raw()),
            (
                fixture.worker.thread_logical_time.clone(),
                fixture.worker.mm_id,
                GlobalRequest::CancelExec(LEADER),
            ),
        )
        .await;
    assert_eq!(result, (None, GlobalResponse::CancelExec(())));
    let before = serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap();
    let mut guest = fixture.guest(Gate::None);
    assert_eq!(
        super::super::reconnect_exec(&mut guest).await,
        Err(reverie::Errno::EINVAL)
    );
    assert_eq!(guest.thread.dettid, WORKER);
    assert_eq!(
        serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
        before
    );
    fixture.assert_running();
}

#[tokio::test]
async fn exec_transfer_rejects_queued_former_and_live_displaced_leader() {
    for queued_former in [false, true] {
        let fixture = Fixture::with_leader_retired(queued_former).await;
        if queued_former {
            fixture.state.sched.lock().unwrap().next_turns[&WORKER]
                .req
                .put(Ok(Resources::new(WORKER)));
        }
        let before = serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap();
        let mut guest = fixture.guest(Gate::None);
        assert_eq!(
            super::super::reconnect_exec(&mut guest).await,
            Err(reverie::Errno::EINVAL)
        );
        assert_eq!(guest.thread.dettid, WORKER);
        assert_eq!(
            serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
            before
        );
        assert!(
            fixture
                .state
                .pending_exec_states
                .lock()
                .unwrap()
                .contains_key(&LEADER)
        );
        assert!(
            fixture
                .state
                .completed_exec_transfers
                .lock()
                .unwrap()
                .is_empty()
        );
        fixture.assert_displaced_leader_receipt(queued_former);
        if !queued_former {
            let payload = deregistration(&fixture.worker);
            let retired = fixture
                .state
                .receive_rpc(
                    Tid::from_raw(LEADER.as_raw()),
                    (
                        fixture.worker.thread_logical_time.clone(),
                        payload.mm,
                        GlobalRequest::RetireExec {
                            thread: payload,
                            signaled: true,
                        },
                    ),
                )
                .await;
            assert_eq!(retired, (None, GlobalResponse::RetireExec(false)));
            assert_eq!(
                serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
                before
            );
            assert!(
                fixture
                    .state
                    .sched
                    .lock()
                    .unwrap()
                    .next_turns
                    .contains_key(&WORKER)
            );
            assert!(
                fixture
                    .state
                    .pending_exec_states
                    .lock()
                    .unwrap()
                    .contains_key(&LEADER)
            );
        }
        fixture.assert_running();
    }
}

async fn commit_without_acknowledgment(fixture: &Fixture) {
    let mut guest = fixture.guest(Gate::AfterCommit);
    {
        let mut reconnect = Box::pin(super::super::reconnect_exec(&mut guest));
        assert!(futures::poll!(&mut reconnect).is_pending());
    }
    assert!(guest.rpc.reached.load(Ordering::SeqCst));
    assert_eq!(guest.thread.dettid, WORKER);
    assert!(
        fixture
            .state
            .completed_exec_transfers
            .lock()
            .unwrap()
            .contains_key(&LEADER)
    );
    fixture.assert_final_clock(LEADER);
    fixture.assert_running();
}

#[tokio::test]
async fn malformed_exec_retirement_cannot_consume_pending_or_completed_transfer() {
    for committed in [false, true] {
        let fixture = Fixture::prepared().await;
        if committed {
            commit_without_acknowledgment(&fixture).await;
        }
        let before = serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap();
        let good = deregistration(&fixture.worker);
        let old_mm = MmId::initial(LEADER);
        for fault in 0..7 {
            let mut payload = good.clone();
            let mut sender = LEADER;
            let mut header_mm = payload.mm;
            match fault {
                0 => sender = WORKER,
                1 => payload.dettid = PARENT,
                2 => payload.detpid = PARENT,
                3 => header_mm = old_mm,
                4 => payload.mm = old_mm,
                5 => {
                    payload.mm = payload.mm.for_exec(LEADER);
                    header_mm = payload.mm;
                }
                6 => payload.dettid = LEADER,
                _ => unreachable!(),
            }
            payload.syscall_count = 999;
            let mut forged_time = fixture.worker.thread_logical_time.clone();
            forged_time.add_syscall_with_cost(999_999);
            let response = fixture
                .state
                .receive_rpc(
                    Tid::from_raw(sender.as_raw()),
                    (
                        forged_time,
                        header_mm,
                        GlobalRequest::RetireExec {
                            thread: payload,
                            signaled: true,
                        },
                    ),
                )
                .await;
            assert_eq!(
                response,
                (None, GlobalResponse::RetireExec(false)),
                "fault {fault}"
            );
            assert_eq!(
                serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
                before
            );
            assert_eq!(
                fixture
                    .state
                    .pending_exec_states
                    .lock()
                    .unwrap()
                    .contains_key(&LEADER),
                !committed
            );
            assert_eq!(
                fixture
                    .state
                    .completed_exec_transfers
                    .lock()
                    .unwrap()
                    .contains_key(&LEADER),
                committed
            );
            assert!(
                !fixture
                    .state
                    .sched
                    .lock()
                    .unwrap()
                    .per_thread_syscalls
                    .contains_key(&WORKER)
            );
            fixture.assert_running();
        }
    }
}

#[tokio::test]
async fn retired_exec_former_rpc_cannot_recreate_clock_or_account_twice() {
    let fixture = Fixture::prepared().await;
    commit_without_acknowledgment(&fixture).await;
    let before = serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap();
    for mm in [MmId::initial(LEADER), fixture.worker.mm_id] {
        let mut payload = deregistration(&fixture.worker);
        payload.mm = mm;
        payload.syscall_count = 999;
        for request in [
            GlobalRequest::GlobalTimeLowerBound,
            GlobalRequest::RequestResources(Resources::new(WORKER), LEADER),
            GlobalRequest::DeregisterThread(payload),
        ] {
            let expected = if matches!(&request, GlobalRequest::DeregisterThread(_)) {
                GlobalResponse::DeregisterThread(())
            } else {
                GlobalResponse::ThreadExited
            };
            let mut forged_time = fixture.worker.thread_logical_time.clone();
            forged_time.add_syscall_with_cost(999_999);
            let mut late = Box::pin(
                fixture
                    .state
                    .receive_rpc(Tid::from_raw(WORKER.as_raw()), (forged_time, mm, request)),
            );
            assert_eq!(
                futures::poll!(&mut late),
                Poll::Ready((None, expected)),
                "a stale request must be rejected immediately, without a grant"
            );
            assert_eq!(
                serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
                before
            );
            assert!(
                !fixture
                    .state
                    .global_time
                    .lock()
                    .unwrap()
                    .contains_thread(WORKER)
            );
            {
                let sched = fixture.state.sched.lock().unwrap();
                assert!(!sched.next_turns.contains_key(&WORKER));
                assert!(!sched.per_thread_syscalls.contains_key(&WORKER));
                assert!(!sched.per_thread_timeslice.contains_key(&WORKER));
                assert!(sched.next_turns[&LEADER].req.try_read().is_none());
            }
            assert!(
                fixture
                    .state
                    .completed_exec_transfers
                    .lock()
                    .unwrap()
                    .contains_key(&LEADER)
            );
            fixture.assert_running();
        }
    }
    // A duplicate successful-exec notification must also fail before accounting.
    let result = fixture
        .state
        .receive_rpc(
            Tid::from_raw(LEADER.as_raw()),
            (
                fixture.worker.thread_logical_time.clone(),
                fixture.worker.mm_id,
                GlobalRequest::ReconnectExec {
                    former: WORKER,
                    process: LEADER,
                },
            ),
        )
        .await;
    assert_eq!(result, (None, GlobalResponse::ReconnectExec(false)));
    assert_eq!(
        serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
        before
    );
}

#[tokio::test]
async fn exec_continuation_applies_relative_grant_once_at_the_carried_current_clock() {
    let fixture = Fixture::prepared().await;
    let mut guest = fixture.guest(Gate::AfterCommit);
    let duration = LogicalTime::from_nanos(1_000_000);
    let now = guest.thread.thread_logical_time.as_nanos();
    let old_deadline = fixture.before_tail + duration;
    guest.thread.end_of_timeslice = Some(old_deadline);
    guest.thread.max_timeslice_end = Some(old_deadline);
    guest.thread.last_rcb_timer = Some(1_234);
    let release = guest.rpc.release.clone();
    {
        let mut reconnect = Box::pin(super::super::reconnect_exec(&mut guest));
        assert!(futures::poll!(&mut reconnect).is_pending());
        // Scheduler replay tests separately prove how the cursor stages this
        // duration. Here a real Go(Value) must reach the preserved local state.
        fixture
            .state
            .sched
            .lock()
            .unwrap()
            .timeslices
            .insert(LEADER, Some(duration));
        release.put(());
        assert!(futures::poll!(&mut reconnect).is_pending());
        let turn = crate::scheduler::do_a_turn_blocking(
            fixture.state.sched.clone(),
            fixture.state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .expect("replacement must receive its duration grant");
        assert_eq!(turn.tid, LEADER);
        assert_eq!(reconnect.await, Ok(()));
    }
    assert_eq!(guest.thread.dettid, LEADER);
    assert_eq!(guest.thread.thread_logical_time.as_nanos(), now);
    assert_eq!(guest.thread.end_of_timeslice, Some(now + duration));
    assert_eq!(guest.thread.max_timeslice_end, Some(now + duration));
    assert_ne!(guest.thread.end_of_timeslice, Some(old_deadline));
    assert_ne!(guest.thread.end_of_timeslice, Some(old_deadline + duration));
    assert_eq!(guest.thread.last_rcb_timer, Some(1_234));
    assert_eq!(fixture.state.sched.lock().unwrap().turn, 1);
    assert!(
        !fixture
            .state
            .sched
            .lock()
            .unwrap()
            .timeslices
            .contains_key(&LEADER)
    );
    fixture.assert_final_clock(LEADER);
}

#[tokio::test]
async fn reused_exec_worker_start_waits_for_parent_registration_before_clock_or_grant() {
    let fixture = Fixture::prepared().await;
    let mut parent = fixture.guest(Gate::None);
    {
        let mut reconnect = Box::pin(super::super::reconnect_exec(&mut parent));
        assert!(futures::poll!(&mut reconnect).is_pending());
        let turn = crate::scheduler::do_a_turn_blocking(
            fixture.state.sched.clone(),
            fixture.state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .unwrap();
        assert_eq!(turn.tid, LEADER);
        assert_eq!(reconnect.await, Ok(()));
    }
    assert_eq!(parent.thread.dettid, LEADER);
    let old_mm = MmId::initial(LEADER);
    let new_mm = parent.thread.mm_id;
    assert_ne!(old_mm, new_mm);
    let before = serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap();
    for mm in [old_mm, new_mm] {
        let rejected = fixture
            .state
            .receive_rpc(
                Tid::from_raw(WORKER.as_raw()),
                (
                    parent.thread.thread_logical_time.clone(),
                    mm,
                    GlobalRequest::GlobalTimeLowerBound,
                ),
            )
            .await;
        assert_eq!(rejected, (None, GlobalResponse::ThreadExited));
        assert_eq!(
            serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
            before
        );
    }

    // Construct the new child through the Tool's real inheritance path. Its
    // reused numeric TID does not authorize either clock publication or startup.
    let flags = CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM;
    parent.thread.clone_flags = Some(flags);
    let mut child = fixture.tool.init_thread_state(
        Tid::from_raw(WORKER.as_raw()),
        Some((Tid::from_raw(LEADER.as_raw()), &parent.thread)),
    );
    parent.thread.clone_flags = None;
    child.detpid = Some(LEADER);
    child.thread_start_entered = true;
    assert_eq!(child.mm_id, new_mm);
    let mut startup = Box::pin(fixture.state.receive_rpc(
        Tid::from_raw(WORKER.as_raw()),
        (
            child.thread_logical_time.clone(),
            new_mm,
            GlobalRequest::StartNewThread(WORKER, LEADER, None, None),
        ),
    ));
    for _ in 0..2 {
        assert!(futures::poll!(&mut startup).is_pending());
        assert_eq!(
            serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
            before
        );
        assert!(
            !fixture
                .state
                .global_time
                .lock()
                .unwrap()
                .contains_thread(WORKER)
        );
        let sched = fixture.state.sched.lock().unwrap();
        assert!(sched.transferred_exec_tid_requires_registration(WORKER));
        assert!(!sched.next_turns.contains_key(&WORKER));
        assert_eq!(sched.turn, 1);
    }

    // The authenticated parent registers the actual clone and then parks on
    // ParentContinue. A higher child priority makes the next grant unambiguous.
    let mut registration = Box::pin(fixture.state.receive_rpc(
        Tid::from_raw(LEADER.as_raw()),
        (
            parent.thread.thread_logical_time.clone(),
            new_mm,
            GlobalRequest::CreateChildThread(
                WORKER,
                LEADER,
                0,
                Some(flags),
                0,
                None,
                Some(DEFAULT_PRIORITY - 1),
            ),
        ),
    ));
    assert!(futures::poll!(&mut registration).is_pending());
    {
        let sched = fixture.state.sched.lock().unwrap();
        assert!(!sched.transferred_exec_tid_requires_registration(WORKER));
        assert!(sched.next_turns.contains_key(&WORKER));
        assert!(sched.rpc_incarnation_matches(WORKER, new_mm));
        assert!(!sched.rpc_incarnation_matches(WORKER, old_mm));
        assert_eq!(sched.turn, 1);
    }
    assert!(
        !fixture
            .state
            .global_time
            .lock()
            .unwrap()
            .contains_thread(WORKER)
    );
    assert!(futures::poll!(&mut startup).is_pending());
    assert!(futures::poll!(&mut startup).is_pending());
    assert!(
        fixture.state.sched.lock().unwrap().next_turns[&WORKER]
            .req
            .try_read()
            .is_some()
    );
    let child_turn = crate::scheduler::do_a_turn_blocking(
        fixture.state.sched.clone(),
        fixture.state.global_time.clone(),
        &Err(crate::scheduler::SkipTurn),
    )
    .await
    .expect("registered reused newborn must receive a real grant");
    assert_eq!(child_turn.tid, WORKER);
    assert_eq!(startup.await, (None, GlobalResponse::StartNewThread(None)));
    assert_eq!(fixture.state.sched.lock().unwrap().turn, 2);
    let total = fixture.before_tail + LogicalTime::from_nanos(497);
    assert_eq!(fixture.state.global_time.lock().unwrap().as_nanos(), total);
    let allowed = fixture
        .state
        .receive_rpc(
            Tid::from_raw(WORKER.as_raw()),
            (
                child.thread_logical_time.clone(),
                new_mm,
                GlobalRequest::GlobalTimeLowerBound,
            ),
        )
        .await;
    assert_eq!(allowed, (None, GlobalResponse::GlobalTimeLowerBound(total)));

    let registered_clock =
        serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap();
    for request in [
        GlobalRequest::StartNewThread(WORKER, LEADER, None, None),
        GlobalRequest::GlobalTimeLowerBound,
    ] {
        let mut forged_time = child.thread_logical_time.clone();
        forged_time.add_syscall_with_cost(999_999);
        let mut stale = Box::pin(fixture.state.receive_rpc(
            Tid::from_raw(WORKER.as_raw()),
            (forged_time, old_mm, request),
        ));
        assert_eq!(
            futures::poll!(&mut stale),
            Poll::Ready((None, GlobalResponse::ThreadExited))
        );
        assert_eq!(
            serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
            registered_clock
        );
        assert_eq!(fixture.state.sched.lock().unwrap().turn, 2);
    }

    // Complete both genuine owners so this test does not abandon the parent's
    // parked registration future after observing the child's successful start.
    consume(&fixture.state, &fixture.tool, WORKER, child).await;
    let parent_turn = crate::scheduler::do_a_turn_blocking(
        fixture.state.sched.clone(),
        fixture.state.global_time.clone(),
        &Err(crate::scheduler::SkipTurn),
    )
    .await
    .unwrap();
    assert_eq!(parent_turn.tid, LEADER);
    assert_eq!(
        registration.await,
        (None, GlobalResponse::CreateChildThread(None))
    );
    assert_eq!(fixture.state.sched.lock().unwrap().turn, 3);
    assert_eq!(fixture.state.global_time.lock().unwrap().as_nanos(), total);

    for flags in [
        CloneFlags::CLONE_VFORK,
        CloneFlags::CLONE_VFORK | CloneFlags::CLONE_VM,
    ] {
        reused_exec_worker_vfork_registration_requires_creation_mode(flags, false).await;
    }
    reused_exec_worker_vfork_registration_requires_creation_mode(CloneFlags::empty(), true).await;
}

async fn reused_exec_worker_vfork_registration_requires_creation_mode(
    valid_flags: CloneFlags,
    backend_serializes_fork_children: bool,
) {
    let fixture = Fixture::configured(true, backend_serializes_fork_children).await;
    let mut parent = fixture.guest(Gate::None);
    {
        let mut reconnect = Box::pin(super::super::reconnect_exec(&mut parent));
        assert!(futures::poll!(&mut reconnect).is_pending());
        let turn = crate::scheduler::do_a_turn_blocking(
            fixture.state.sched.clone(),
            fixture.state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .unwrap();
        assert_eq!(turn.tid, LEADER);
        assert_eq!(reconnect.await, Ok(()));
    }
    let parent_mm = parent.thread.mm_id;
    let child_mm = MmId::for_clone(
        parent_mm,
        WORKER,
        valid_flags.contains(CloneFlags::CLONE_VM),
    );
    let child_time = parent.thread.thread_logical_time.clone_for_child();
    let mut resources = Resources::new(LEADER);
    resources.insert(
        ResourceID::BlockingVfork(ExternalOpId::new(LEADER, 1)),
        Permission::RW,
    );
    let mut blocking = Box::pin(fixture.state.receive_rpc(
        Tid::from_raw(LEADER.as_raw()),
        (
            parent.thread.thread_logical_time.clone(),
            parent_mm,
            GlobalRequest::RequestResources(resources, LEADER),
        ),
    ));
    assert!(futures::poll!(&mut blocking).is_pending());
    let background = crate::scheduler::do_a_turn_blocking(
        fixture.state.sched.clone(),
        fixture.state.global_time.clone(),
        &Err(crate::scheduler::SkipTurn),
    )
    .await;
    assert!(background.is_err());
    assert_eq!(
        blocking.await,
        (None, GlobalResponse::RequestResources(ResumeStatus::Normal))
    );
    let before = serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap();
    let before_total = fixture.state.global_time.lock().unwrap().as_nanos();
    assert_eq!(fixture.state.sched.lock().unwrap().turn, 2);

    // The parent/mm proof is genuinely valid here. Only the creation mode is
    // wrong: ordinary clones cannot borrow a vfork grant, and a backend that
    // serializes process forks does not thereby authorize thread clones.
    let wrong_flags = if backend_serializes_fork_children {
        CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM
    } else {
        valid_flags & !CloneFlags::CLONE_VFORK
    };
    let wrong_mm = MmId::for_clone(
        parent_mm,
        WORKER,
        wrong_flags.contains(CloneFlags::CLONE_VM),
    );
    {
        let sched = fixture.state.sched.lock().unwrap();
        assert!(sched.pending_vfork_registration_matches(
            LEADER,
            LEADER,
            WORKER,
            wrong_mm,
            wrong_flags.contains(CloneFlags::CLONE_VM),
        ));
        assert!(sched.transferred_exec_tid_requires_registration(WORKER));
    }
    let mut forged_time = child_time.clone();
    forged_time.add_syscall_with_cost(999_999);
    let rejected = fixture
        .state
        .receive_rpc(
            Tid::from_raw(WORKER.as_raw()),
            (
                forged_time,
                wrong_mm,
                GlobalRequest::CreateVforkChildThread(
                    LEADER,
                    LEADER,
                    WORKER,
                    0,
                    wrong_flags,
                    libc::SIGCHLD,
                    Some(DEFAULT_PRIORITY),
                ),
            ),
        )
        .await;
    assert_eq!(rejected, (None, GlobalResponse::ThreadExited));
    assert_eq!(
        serde_json::to_value(&*fixture.state.global_time.lock().unwrap()).unwrap(),
        before
    );
    assert!(
        !fixture
            .state
            .global_time
            .lock()
            .unwrap()
            .contains_thread(WORKER)
    );
    {
        let sched = fixture.state.sched.lock().unwrap();
        assert!(sched.transferred_exec_tid_requires_registration(WORKER));
        assert!(!sched.next_turns.contains_key(&WORKER));
        assert_eq!(sched.turn, 2);
        assert!(sched.pending_vfork_registration_matches(
            LEADER,
            LEADER,
            WORKER,
            child_mm,
            valid_flags.contains(CloneFlags::CLONE_VM),
        ));
    }

    // The same, still-unconsumed grant admits the valid raw-vfork mode or the
    // configured serialized-process-fork mode, without double-counting ancestry.
    let created = fixture
        .state
        .receive_rpc(
            Tid::from_raw(WORKER.as_raw()),
            (
                child_time.clone(),
                child_mm,
                GlobalRequest::CreateVforkChildThread(
                    LEADER,
                    LEADER,
                    WORKER,
                    0,
                    valid_flags,
                    libc::SIGCHLD,
                    Some(DEFAULT_PRIORITY),
                ),
            ),
        )
        .await;
    assert_eq!(created, (None, GlobalResponse::CreateChildThread(None)));
    assert_eq!(
        fixture.state.global_time.lock().unwrap().as_nanos(),
        before_total
    );
    assert_eq!(
        fixture
            .state
            .global_time
            .lock()
            .unwrap()
            .threads_time(WORKER),
        child_time.as_nanos()
    );
    {
        let sched = fixture.state.sched.lock().unwrap();
        assert!(!sched.transferred_exec_tid_requires_registration(WORKER));
        assert_eq!(sched.registered_process(WORKER), Some(WORKER));
        assert!(sched.rpc_incarnation_matches(WORKER, child_mm));
        assert!(!sched.pending_vfork_registration_matches(
            LEADER,
            LEADER,
            WORKER,
            child_mm,
            valid_flags.contains(CloneFlags::CLONE_VM),
        ));
        assert_eq!(sched.turn, 2);
    }
    let mut startup = Box::pin(fixture.state.receive_rpc(
        Tid::from_raw(WORKER.as_raw()),
        (
            child_time,
            child_mm,
            GlobalRequest::StartNewThread(WORKER, WORKER, None, None),
        ),
    ));
    assert!(futures::poll!(&mut startup).is_pending());
    assert!(futures::poll!(&mut startup).is_pending());
    let turn = crate::scheduler::do_a_turn_blocking(
        fixture.state.sched.clone(),
        fixture.state.global_time.clone(),
        &background,
    )
    .await
    .expect("authorized reused vfork child must receive its first turn");
    assert_eq!(turn.tid, WORKER);
    assert_eq!(startup.await, (None, GlobalResponse::StartNewThread(None)));
    assert_eq!(fixture.state.sched.lock().unwrap().turn, 3);
    assert_eq!(
        fixture.state.global_time.lock().unwrap().as_nanos(),
        before_total
    );
}
