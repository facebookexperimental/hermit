/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

struct LegacyDeadlineGuest<'a> {
    inner: ExternalRegistrationGuest<'a>,
    clock_reads: usize,
    reported_rcbs: u64,
    timers: Vec<reverie::TimerSchedule>,
    responses: Mutex<Vec<GlobalResponse>>,
}

impl<'a> std::ops::Deref for LegacyDeadlineGuest<'a> {
    type Target = ExternalRegistrationGuest<'a>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for LegacyDeadlineGuest<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for LegacyDeadlineGuest<'_> {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let response = self.inner.send_rpc(message).await;
        self.responses.lock().unwrap().push(response.1.clone());
        response
    }
    fn config(&self) -> &Config {
        self.inner.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for LegacyDeadlineGuest<'_> {
    type Memory = reverie::syscalls::LocalMemory;
    type Stack = ExternalRegistrationStack;
    fn tid(&self) -> reverie::Pid {
        self.inner.tid()
    }
    fn pid(&self) -> reverie::Pid {
        self.inner.pid()
    }
    fn ppid(&self) -> Option<reverie::Pid> {
        self.inner.ppid()
    }
    fn memory(&self) -> Self::Memory {
        self.inner.memory()
    }
    fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
        &mut self.inner.thread
    }
    fn thread_state(&self) -> &crate::ThreadState<()> {
        &self.inner.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        // Synthetic registers are inspected, never executed by this fixture.
        let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        regs.rip = 1;
        regs
    }
    async fn stack(&mut self) -> Self::Stack {
        self.inner.stack().await
    }
    async fn daemonize(&mut self) {
        self.inner.daemonize().await
    }
    async fn inject<S: reverie::syscalls::SyscallInfo>(
        &mut self,
        syscall: S,
    ) -> Result<i64, reverie::syscalls::Errno> {
        self.inner.inject(syscall).await
    }
    async fn tail_inject<S: reverie::syscalls::SyscallInfo>(
        &mut self,
        syscall: S,
    ) -> reverie::Never {
        self.inner.tail_inject(syscall).await
    }
    fn set_timer(&mut self, schedule: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        self.timers.push(schedule);
        Ok(())
    }
    fn set_timer_precise(
        &mut self,
        schedule: reverie::TimerSchedule,
    ) -> Result<(), reverie::Error> {
        self.timers.push(schedule);
        Ok(())
    }
    fn read_clock(&mut self) -> Result<u64, reverie::Error> {
        self.clock_reads += 1;
        Ok(self.reported_rcbs)
    }
}

// Source-reachable phase counterexample, with no guest or physical timer.
// The actual prehook and posthook see the same clock; only posthook's
// existing epsilon rule ends the slice. A joint time/RCB match cannot
// independently identify this phase from a bare legacy history entry.
#[tokio::test]
async fn legacy_history_same_time_timer_checkin_has_distinct_pre_and_post_expiry() {
    use reverie::Tool;
    let mut config = Config {
        chaos: true,
        max_timeslice: std::num::NonZeroU64::new(101),
        target_timeslice: std::num::NonZeroU64::new(1_000_000),
        ..Config::default()
    }
    .with_backend(|backend| backend.delivers_preemption_timer_events = true);
    config.validate();
    assert!(config.use_preemption_timer());
    let (config, state, tid, process) = cancellation_test_state_with_config(config);
    install_test_registration(&state, tid, Ivar::new());
    let mut thread = crate::ThreadState::new(tid, &config, ());
    thread.detpid = Some(process);
    let start = thread.thread_logical_time.as_nanos();
    thread.next_timeslice(&config);
    assert_eq!(
        thread.end_of_timeslice,
        Some(start + Duration::from_nanos(101))
    );
    assert_eq!(thread.max_timeslice_end, thread.end_of_timeslice);
    // Actual posthook PMU conversion floors101ns at10ns/RCB to10RCBs.
    let due = (thread.max_timeslice_end.unwrap() - start).into_rcbs_with_multiplier(1.0);
    assert_eq!(due, 10);
    thread.last_rcb_timer = Some(due);
    let mut guest = LegacyDeadlineGuest {
        inner: ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: false,
            ppid: None,
            private_memory: None,
        },
        clock_reads: 0,
        reported_rcbs: due,
        timers: Vec::new(),
        responses: Mutex::new(Vec::new()),
    };
    let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
    tool.pre_handler_hook(&mut guest, true, crate::CheckIn::Event)
        .await;
    let at = guest.thread.thread_logical_time.as_nanos();
    assert_eq!(at, start + Duration::from_nanos(100));
    assert_eq!(guest.thread.committed_clock_value, due);
    assert!(!guest.thread.timeslice_expired());
    assert!(guest.requests.lock().unwrap().is_empty());
    let mut post = Box::pin(tool.post_handler_hook(&mut guest, crate::CheckIn::Event));
    for _ in 0..8 {
        assert!(futures::poll!(&mut post).is_pending());
        if state.sched.lock().unwrap().next_turns[&tid]
            .req
            .try_read()
            .is_some()
        {
            break;
        }
    }
    let request = state.sched.lock().unwrap().next_turns[&tid]
        .req
        .try_read()
        .unwrap()
        .unwrap();
    let resource = request.resources.keys().next().unwrap();
    assert!(
        matches!(resource, ResourceID::PriorityChangePoint(_, time, rcbs, _) if *time == at && *rcbs == due)
    );
    assert_eq!(request.resources.len(), 1);
    let skipped = crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &Err(crate::scheduler::SkipTurn),
    )
    .await;
    assert!(skipped.is_err());
    assert!(
        state.sched.lock().unwrap().next_turns[&tid]
            .resp
            .try_read()
            .is_none()
    );
    let granted = crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &skipped,
    )
    .await
    .unwrap();
    assert!(granted.resources.is_empty());
    post.await;
    assert_eq!(guest.thread.thread_logical_time.as_nanos(), at);
    assert_eq!(guest.thread.committed_clock_value, due);
    assert_eq!(guest.clock_reads, 1);
    assert_eq!(guest.timers.len(), 1);
}

// The actual hooks, TraceSchedEvent and resource RPC drive this fixture.
// No physical guest or timer runs; the original syscall clock charge is real.
async fn legacy_callback_close_trial(
    input: Option<&std::path::Path>,
) -> (
    String,
    Vec<crate::preemptions::callback_history::Origin>,
    LogicalTime,
    u64,
) {
    use reverie::Tool;

    use crate::preemptions::callback_history::Startup;
    use crate::slice_boundary::callbacks::Callback;
    let mut config = Config {
        chaos: input.is_none(),
        record_preemptions: true,
        sequentialize_threads: true,
        replay_schedule_from: input.map(std::path::Path::to_path_buf),
        max_timeslice: std::num::NonZeroU64::new(5_000_000),
        target_timeslice: std::num::NonZeroU64::new(1_000_000),
        ..Config::default()
    }
    .with_backend(|backend| backend.delivers_preemption_timer_events = true);
    config.validate();
    let (config, state, tid, process) = cancellation_test_state_with_config(config);
    install_test_registration(&state, tid, Ivar::new());
    state
        .sched
        .lock()
        .unwrap()
        .preemption_writer
        .as_mut()
        .unwrap()
        .register_thread(tid, DEFAULT_PRIORITY);
    let mut thread = crate::ThreadState::new(tid, &config, ());
    thread.detpid = Some(process);
    thread.next_timeslice(&config);
    if input.is_none() {
        thread.end_of_timeslice =
            Some(thread.thread_logical_time.as_nanos() + Duration::from_nanos(10_000));
    } else {
        let points = state.sched.lock().unwrap().callback_checkpoints[&tid]
            .iter()
            .cloned()
            .collect();
        thread.callback_history.install(Startup {
            checkpoints: points,
            ..Startup::default()
        });
    }
    let mut guest = LegacyDeadlineGuest {
        inner: ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: false,
            ppid: None,
            private_memory: None,
        },
        clock_reads: 0,
        reported_rcbs: 10,
        timers: Vec::new(),
        responses: Mutex::new(Vec::new()),
    };
    let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
    let token = tool
        .pre_handler_hook_observed(
            &mut guest,
            true,
            crate::CheckIn::Event,
            Some(Callback::Syscall(Sysno::close)),
        )
        .await;
    assert!(!guest.thread.timeslice_expired());
    assert!(
        state.sched.lock().unwrap().next_turns[&tid]
            .req
            .try_read()
            .is_none()
    );
    let mut pre = SchedEvent::syscall(
        tid,
        Sysno::close,
        detcore_model::schedule::SyscallPhase::Prehook,
    );
    pre.end_time = Some(guest.thread.thread_logical_time.as_nanos());
    super::trace_schedevent(&mut guest, pre, false).await;
    guest.thread.thread_logical_time.add_syscall();
    let mut post_event = SchedEvent::syscall(
        tid,
        Sysno::close,
        detcore_model::schedule::SyscallPhase::Posthook,
    );
    post_event.end_time = Some(guest.thread.thread_logical_time.as_nanos());
    super::trace_schedevent(&mut guest, post_event, false).await;
    let mut post =
        Box::pin(tool.post_handler_hook_observed(&mut guest, crate::CheckIn::Event, token));
    let mut origins = Vec::new();
    let mut previous = Err(crate::scheduler::SkipTurn);
    let mut complete = false;
    for _ in 0..32 {
        if futures::poll!(&mut post).is_ready() {
            complete = true;
            break;
        }
        let posted = state.sched.lock().unwrap().next_turns[&tid].req.try_read();
        let Some(posted) = posted else {
            tokio::task::yield_now().await;
            continue;
        };
        let posted = posted.unwrap();
        let origin = posted
            .callback_checkpoint
            .clone()
            .expect("every actual loop expiry is annotated");
        assert_eq!(origin.callback, Callback::Syscall(Sysno::close));
        assert_eq!(
            origin.site,
            crate::preemptions::callback_history::Site::Posthook
        );
        assert_eq!(origin.progress, 12);
        assert_eq!(origin.rcbs, 10);
        origins.push(*origin);
        let skipped = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &previous,
        )
        .await;
        assert!(skipped.is_err());
        assert!(
            state.sched.lock().unwrap().next_turns[&tid]
                .resp
                .try_read()
                .is_none()
        );
        let empty = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &skipped,
        )
        .await
        .unwrap();
        assert!(empty.resources.is_empty());
        previous = Ok(empty);
    }
    assert!(
        complete,
        "bounded callback fixture must finish its actual expiry loop"
    );
    drop(post);
    let local_before = guest.thread.thread_logical_time.as_nanos();
    let global_before = state.global_time.lock().unwrap().as_nanos();
    // The next real request accounts for the just-completed empty grant.
    // Scheduler time belongs to the aggregate clock, not this thread's
    // local deadline. Do not inject local time to force another expiry.
    let mut tail = Box::pin(super::resource_request(&mut guest, Resources::new(tid)));
    assert!(futures::poll!(&mut tail).is_pending());
    let granted = crate::scheduler::do_a_turn_blocking(
        state.sched.clone(),
        state.global_time.clone(),
        &previous,
    )
    .await
    .unwrap();
    assert!(granted.resources.is_empty());
    let _ = tail.await;
    assert_eq!(guest.thread.thread_logical_time.as_nanos(), local_before);
    assert_eq!(
        (state.global_time.lock().unwrap().as_nanos() - global_before).as_nanos(),
        500_000
    );
    // The replay has spent its checkpoint through the real selected request.
    // A subsequent actual event must still advance local/global progress.
    super::trace_schedevent(&mut guest, SchedEvent::branches(tid, 1), false).await;
    assert_eq!(guest.thread.callback_history.progress().count, 13);
    let mut scheduler = state.sched.lock().unwrap();
    assert!(scheduler.callback_history_active);
    assert_eq!(scheduler.callback_progress[&tid], guest.thread.callback_history.progress());
    assert!(
        scheduler
            .callback_checkpoints
            .values()
            .all(|points| points.is_empty())
    );
    let encoded = scheduler.preemption_writer.take().unwrap().into_string();
    (
        encoded,
        origins,
        guest.thread.thread_logical_time.as_nanos(),
        scheduler.turn,
    )
}

#[tokio::test]
async fn legacy_callback_history_replays_close_expiry_and_original_global_charge() {
    let (encoded, origins, time, turns) = legacy_callback_close_trial(None).await;
    eprintln!(
        "recorded callback expiries: {} {:?}; final={time:?}, turns={turns}",
        origins.len(),
        origins.iter().map(|origin| origin.time).collect::<Vec<_>>()
    );
    assert_eq!(
        origins.len(),
        1,
        "this actual empty grant changes global time, not the local deadline"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.json");
    let mut record =
        crate::preemptions::boundary_recording::Recording::decode(encoded.as_bytes()).unwrap();
    record.strip_times();
    std::fs::write(&path, record.encode().unwrap()).unwrap();
    let (_, actual, actual_time, actual_turns) = legacy_callback_close_trial(Some(&path)).await;
    assert_eq!(actual, origins);
    assert_eq!(actual_time, time);
    assert_eq!(actual_turns, turns);
}

async fn start_history_image(
    tool: &Detcore,
    guest: &mut LegacyDeadlineGuest<'_>,
    expected: &[ResourceID],
) {
    use reverie::Tool;
    let state = guest.global;
    let tid = guest.thread.dettid;
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut start = std::pin::pin!(tool.handle_thread_start(guest));
        assert!(futures::poll!(start.as_mut()).is_pending());
        let grants = async {
            let mut last = Err(crate::scheduler::SkipTurn);
            for resource in expected {
                let granted = crate::scheduler::do_a_turn_blocking(
                    state.sched.clone(),
                    state.global_time.clone(),
                    &last,
                )
                .await
                .unwrap();
                assert_eq!(granted.tid, tid);
                assert!(granted.resources.contains_key(resource));
                last = Ok(granted);
            }
        };
        let (started, ()) = tokio::join!(start, grants);
        started.unwrap();
    })
    .await
    .expect("actual startup remains bounded");
}

#[tokio::test]
async fn legacy_callback_history_restores_actual_progress_after_same_leader_reload() {
    use reverie::Tool;
    // Exercise actual loaded formats and startup replies, not a manually
    // enabled State. Ordinary modes must retain completely inactive history.
    for (recording, chaos, replay, annotated) in [
        (true, true, false, false),
        (false, false, true, true),
        (false, true, true, true),
        (false, false, false, false),
        (true, false, false, false),
        (false, false, true, false),
        (true, true, true, false),
    ] {
        let active = annotated || (recording && chaos && !replay);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reload.json");
        let leader = super::ROOT_DETPID;
        if replay {
            let mut writer = crate::preemptions::PreemptionWriter::new(None);
            writer.register_thread(leader, DEFAULT_PRIORITY);
            writer.insert_schedevent(SchedEvent::branches(leader, 10));
            writer.insert_schedevent(SchedEvent::branches(leader, 7));
            if annotated {
                use crate::preemptions::callback_history::{Origin, Site};
                use crate::slice_boundary::callbacks::Callback;
                let at = LogicalTime::from_nanos(100);
                writer.insert_reprioritization(leader, at, 0, DEFAULT_PRIORITY, DEFAULT_PRIORITY);
                writer.insert_callback_checkpoint(leader, Origin {
                    callback: Callback::PmuTimer,
                    site: Site::Prehook,
                    progress: 17,
                    last_op: Some(Op::Branch),
                    time: at,
                    rcbs: 0,
                });
            }
            std::fs::write(&path, writer.into_string()).unwrap();
        }
        let mut config = Config {
            sequentialize_threads: true,
            max_timeslice: None,
            no_rcb_time: true,
            chaos,
            replay_schedule_from: replay.then_some(path),
            record_preemptions: recording,
            ..Config::default()
        }
        .with_backend(|backend| backend.needs_killed_thread_rpc_cancellation = true);
        config.validate();
        let state = GlobalState::initialize(&config, false);
        assert_eq!(state.sched.lock().unwrap().callback_history_active, active);
        let loaded_points = state.sched.lock().unwrap().callback_checkpoints
            .get(&leader).map(|points| points.iter().cloned().collect::<Vec<_>>()).unwrap_or_default();
        assert_eq!(loaded_points.len(), usize::from(annotated));
        let tool: Detcore = Detcore::new(Tid::from_raw(leader.as_raw()), &config);
        let new_guest = || LegacyDeadlineGuest {
            inner: ExternalRegistrationGuest {
                global: &state,
                config: &config,
                thread: tool.init_thread_state(Tid::from_raw(leader.as_raw()), None),
                requests: Mutex::new(Vec::new()),
                post_exec: true,
                ppid: None,
                private_memory: None,
            },
            clock_reads: 0,
            reported_rcbs: 0,
            timers: Vec::new(),
            responses: Mutex::new(Vec::new()),
        };
        let mut old = new_guest();
        old.thread.detpid = Some(leader);
        start_history_image(
            &tool,
            &mut old,
            &[
                ResourceID::ParentContinue {
                    parent: leader,
                    child: leader,
                },
                ResourceID::MemAddrSpace(leader),
            ],
        )
        .await;
        assert_eq!(old.thread.callback_history.is_active(), active);
        let inactive = bincode::serde::encode_to_vec(&old.thread.callback_history, bincode::config::legacy()).unwrap();
        let mut event = SchedEvent::branches(leader, 10);
        event.end_time = Some(old.thread.thread_logical_time.as_nanos());
        super::trace_schedevent(&mut old, event, false).await;
        let observed = old.thread.callback_history.progress();
        if active {
            assert_eq!(observed.count, 10);
            assert_eq!(state.sched.lock().unwrap().callback_progress[&leader], observed);
        } else {
            assert_eq!(bincode::serde::encode_to_vec(&old.thread.callback_history, bincode::config::legacy()).unwrap(), inactive);
            assert!(state.sched.lock().unwrap().callback_progress.is_empty());
        }
        let preserved_history = old.thread.callback_history.clone();
        let old_mm = old.thread.mm_id;
        super::prepare_exec(&mut old, old_mm, Default::default(), Vec::new(), None).await;
        // A real future alarm supplies a grant hint in ordinary mode.
        // The internal record/replay mode is off in both run configurations.
        let deadline = old.thread.thread_logical_time.as_nanos() + Duration::from_secs(10);
        state
            .sched
            .lock()
            .unwrap()
            .blocked
            .timed_waiters
            .insert_alarm(
                deadline,
                leader,
                leader,
                nix::sys::signal::Signal::SIGALRM,
                LogicalTime::ZERO,
            );
        drop(old);
        let mut successor = new_guest();
        successor.thread.detpid = Some(leader);
        assert_eq!(successor.thread.callback_history.progress().count, 0);
        start_history_image(&tool, &mut successor, &[ResourceID::MemAddrSpace(leader)]).await;
        assert_eq!(successor.thread.mm_id, old_mm.for_exec(leader));
        assert_eq!(successor.thread.callback_history.progress(), observed);
        assert_eq!(successor.thread.callback_history, preserved_history);
        assert_eq!(successor.thread.callback_history.is_active(), active);
        {
            let responses = successor.responses.lock().unwrap();
            let carries_history = responses.iter().any(|response| {
                    let GlobalResponse::Granted(response) = response else {
                        return false;
                    };
                    let GlobalResponse::WithDeadline(_, response) = response.as_ref() else {
                        return false;
                    };
                    let GlobalResponse::WithCallbackHistory(startup, response) = response.as_ref()
                    else {
                        return false;
                    };
                    startup.progress == observed
                        && startup.checkpoints == loaded_points
                        && matches!(response.as_ref(), GlobalResponse::StartNewThread(None))
                });
            assert_eq!(carries_history, active, "actual startup carries and consumes active history with its deadline: {responses:?}");
        }
        let mut event = SchedEvent::branches(leader, 7);
        event.end_time = Some(successor.thread.thread_logical_time.as_nanos());
        super::trace_schedevent(&mut successor, event, false).await;
        if active {
            assert_eq!(successor.thread.callback_history.progress().count, 17);
            assert_eq!(state.sched.lock().unwrap().callback_progress[&leader].count, 17);
        } else {
            assert_eq!(bincode::serde::encode_to_vec(&successor.thread.callback_history, bincode::config::legacy()).unwrap(), inactive);
            assert!(state.sched.lock().unwrap().callback_progress.is_empty());
        }
        if recording {
            let encoded = state
                .sched
                .lock()
                .unwrap()
                .preemption_writer
                .take()
                .unwrap()
                .into_string();
            let record: crate::preemptions::PreemptionRecord =
                serde_json::from_str(&encoded).unwrap();
            record.validate().unwrap();
            assert_eq!(
                record
                    .schedevents()
                    .iter()
                    .filter(|event| event.dettid == leader)
                    .map(|event| u64::from(event.count))
                    .sum::<u64>(),
                17
            );
        }
    }
}

#[tokio::test]
async fn legacy_callback_history_old_chaos_replay_record_flags_make_no_authority() {
    use reverie::Tool;

    use crate::preemptions::callback_history::Site;
    use crate::slice_boundary::callbacks::Callback;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.json");
    std::fs::write(&path, r#"{"per_thread":{},"global":[]}"#).unwrap();
    let mut config = Config {
        chaos: true,
        record_preemptions: true,
        replay_schedule_from: Some(path),
        ..Config::default()
    };
    config.validate();
    let (config, state, tid, process) = cancellation_test_state_with_config(config);
    let mut thread = crate::ThreadState::new(tid, &config, ());
    thread.detpid = Some(process);
    let mut guest = ExternalRegistrationGuest {
        global: &state,
        config: &config,
        thread,
        requests: Mutex::new(Vec::new()),
        post_exec: false,
        ppid: None,
        private_memory: None,
    };
    let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
    let token = crate::preemptions::callback_history::begin(
        &mut guest.thread,
        Some(Callback::Syscall(Sysno::close)),
    )
    .unwrap();
    assert!(
        tool.record_callback_origin(&guest, &token, Site::Posthook)
            .is_none()
    );
    assert!(state.sched.lock().unwrap().callback_checkpoints.is_empty());
}

// Drive the actual hook/event RPC, rather than assigning a future deadline in
// the fixture. The replay cursor authorizes the next 90 branches before the
// software checkpoint at this callback performs its administrative turn.
async fn legacy_callback_branch_deadline_trial(annotated: bool, owner_switch: bool) {
    use reverie::Tool;
    use crate::preemptions::callback_history::{Origin, Site, Startup};
    use crate::slice_boundary::callbacks::Callback;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("branch-deadline.json");
    let tid = DetTid::from_raw(17);
    let peer = DetTid::from_raw(18);
    let mut config = Config {
        sequentialize_threads: true,
        replay_schedule_from: Some(path.clone()),
        max_timeslice: std::num::NonZeroU64::new(5_000_000),
        target_timeslice: std::num::NonZeroU64::new(1_000_000),
        ..Config::default()
    }
    .with_backend(|backend| backend.delivers_preemption_timer_events = true);
    config.validate();
    let start = crate::ThreadState::new(tid, &config, ())
        .thread_logical_time
        .as_nanos();
    let at = start + Duration::from_nanos(100);
    let deadline = at + Duration::from_nanos(900);
    let mut writer = crate::preemptions::PreemptionWriter::new(None);
    writer.register_thread(tid, DEFAULT_PRIORITY);
    writer.insert_schedevent(SchedEvent::branches(tid, 10));
    let peer_event = SchedEvent::syscall(
        peer,
        Sysno::getpid,
        detcore_model::schedule::SyscallPhase::Posthook,
    );
    if owner_switch {
        writer.register_thread(peer, DEFAULT_PRIORITY);
        writer.insert_schedevent(peer_event.clone());
    }
    writer.insert_schedevent(SchedEvent::branches(tid, 90));
    writer.insert_schedevent(SchedEvent::branches(tid, 1));
    writer.insert_reprioritization(tid, at, 10, DEFAULT_PRIORITY, DEFAULT_PRIORITY);
    if annotated {
        writer.insert_callback_checkpoint(tid, Origin {
            callback: Callback::PmuTimer,
            site: Site::Prehook,
            progress: 10,
            last_op: Some(Op::Branch),
            time: at,
            rcbs: 10,
        });
    }
    std::fs::write(&path, writer.into_string()).unwrap();
    let (config, state, tid, process) = cancellation_test_state_with_config(config);
    install_test_registration(&state, tid, Ivar::new());
    state.past_first_execve.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut thread = crate::ThreadState::new(tid, &config, ());
    thread.detpid = Some(process);
    thread.past_global_first_execve = true;
    thread.next_timeslice(&config);
    if annotated {
    thread.callback_history.install(Startup {
        checkpoints: state.sched.lock().unwrap().callback_checkpoints
            .get(&tid).map(|points| points.iter().cloned().collect()).unwrap_or_default(),
        ..Startup::default()
    });
    }
    let mut guest = LegacyDeadlineGuest {
        inner: ExternalRegistrationGuest {
            global: &state, config: &config, thread,
            requests: Mutex::new(Vec::new()), post_exec: false,
            ppid: None, private_memory: None,
        },
        clock_reads: 0, reported_rcbs: 10, timers: Vec::new(),
        responses: Mutex::new(Vec::new()),
    };
    let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
    let token = {
        let mut pre = Box::pin(tool.pre_handler_hook_observed(
            &mut guest, true, crate::CheckIn::Event, Some(Callback::PmuTimer),
        ));
        let mut previous = Err(crate::scheduler::SkipTurn);
        if owner_switch {
            assert!(futures::poll!(&mut pre).is_pending());
            let request = state.sched.lock().unwrap().next_turns[&tid]
                .req.try_read().unwrap().unwrap();
            assert!(request.resources.contains_key(&ResourceID::TraceReplay));
            state.sched.lock().unwrap().thread_tree.add_child(tid, peer, false);
            install_test_registration(&state, peer, Ivar::new());
            let mut peer_thread = crate::ThreadState::new(peer, &config, ());
            peer_thread.detpid = Some(process);
            let mut peer_guest = ExternalRegistrationGuest {
                global: &state, config: &config, thread: peer_thread,
                requests: Mutex::new(Vec::new()), post_exec: false,
                ppid: None, private_memory: None,
            };
            let mut peer_request = Box::pin(super::resource_request(&mut peer_guest, Resources::new(peer)));
            assert!(futures::poll!(&mut peer_request).is_pending());
            let granted = crate::scheduler::do_a_turn_blocking(
                state.sched.clone(), state.global_time.clone(), &previous,
            ).await.unwrap();
            assert_eq!(granted.tid, peer);
            peer_request.await;
            // The real peer event transfers the cursor back to A, whose real
            // TraceReplay request receives the newly authorized Branch(90).
            peer_guest.thread.thread_logical_time.add_syscall();
            let mut observed_peer = peer_event;
            observed_peer.end_time = Some(peer_guest.thread.thread_logical_time.as_nanos());
            let mut peer_trace = Box::pin(super::trace_schedevent(&mut peer_guest, observed_peer, false));
            assert!(futures::poll!(&mut peer_trace).is_pending());
            previous = Ok(granted);
            let resumed = crate::scheduler::do_a_turn_blocking(
                state.sched.clone(), state.global_time.clone(), &previous,
            ).await.unwrap();
            assert_eq!(resumed.tid, tid);
            assert!(resumed.resources.contains_key(&ResourceID::TraceReplay));
            previous = Ok(resumed);
            // B is legitimately parked until a later B event, outside this
            // finite prefix. Retain its filed request while A continues.
            drop(peer_trace);
        }
        if annotated {
            assert!(futures::poll!(&mut pre).is_pending());
            let request = state.sched.lock().unwrap().next_turns[&tid]
                .req.try_read().unwrap().unwrap();
            assert!(request.callback_checkpoint.is_some());
            let skipped = crate::scheduler::do_a_turn_blocking(
                state.sched.clone(), state.global_time.clone(), &previous,
            ).await;
            assert!(skipped.is_err());
            let empty = crate::scheduler::do_a_turn_blocking(
                state.sched.clone(), state.global_time.clone(), &skipped,
            ).await.unwrap();
            assert_eq!(empty.tid, tid);
            assert!(empty.resources.is_empty());
        }
        pre.await
    };
    assert_eq!(guest.thread.thread_logical_time.as_nanos(), at);
    assert_eq!(guest.thread.end_of_timeslice, Some(deadline), "cursor-owned logical deadline; annotated={annotated}, owner_switch={owner_switch}");
    assert_eq!(guest.thread.max_timeslice_end, Some(deadline));
    assert_eq!(guest.thread.replay_rcb_end, None, "legacy branch guidance does not install strict RCB authority");
    tool.post_handler_hook_observed(&mut guest, crate::CheckIn::Event, token).await;
    assert_eq!(guest.timers.len(), 1);
    assert!(matches!(guest.timers[0], reverie::TimerSchedule::Rcbs(90)));
    assert_eq!(guest.thread.last_rcb_timer, Some(90));
    // Feed the PMU result through the real update/event path, without entering
    // another expiry hook: this proves the next observed branch matches.
    guest.reported_rcbs = 100;
    let events = tool.update_logical_time_rcbs(&mut guest, true).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!((events[0].op, events[0].count), (Op::Branch, 90));
    for event in events { super::trace_schedevent(&mut guest, event, false).await; }
    let scheduler = state.sched.lock().unwrap();
    let replayer = scheduler.replayer.as_ref().unwrap();
    assert!(replayer.desync_counts.values().all(|stats| stats.soft == 0 && stats.hard == 0));
    assert_eq!(replayer.cursor.peek().unwrap().count, 1);
    assert!(scheduler.callback_checkpoints.values().all(|points| points.is_empty()));
}

#[tokio::test]
async fn legacy_callback_checkpoint_preserves_continue_branch_deadline() {
    tokio::time::timeout(Duration::from_secs(3), async {
        for annotated in [false, true] { legacy_callback_branch_deadline_trial(annotated, false).await; }
    }).await.expect("bounded real hook/RPC continuation");
}

#[tokio::test]
async fn legacy_callback_checkpoint_preserves_tracereplay_branch_deadline() {
    tokio::time::timeout(Duration::from_secs(3), async {
        for annotated in [false, true] { legacy_callback_branch_deadline_trial(annotated, true).await; }
    }).await.expect("bounded real hook/RPC owner switch");
}

// Advance only actual filed requests while a hook is pending. A successful
// grant counts as a committed turn; SkipTurn remains a separate observation.
async fn drive_callback_hook<F: std::future::Future>(
    state: &GlobalState,
    tid: DetTid,
    future: F,
    previous: &mut Result<Resources, crate::scheduler::SkipTurn>,
) -> (F::Output, usize, usize) {
    let mut future = std::pin::pin!(future);
    let (mut commits, mut skips) = (0, 0);
    for _ in 0..32 {
        if let std::task::Poll::Ready(output) = futures::poll!(future.as_mut()) {
            return (output, commits, skips);
        }
        if state.sched.lock().unwrap().next_turns[&tid].req.try_read().is_none() {
            tokio::task::yield_now().await;
            continue;
        }
        *previous = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(), state.global_time.clone(), previous,
        ).await;
        if previous.is_ok() { commits += 1; } else { skips += 1; }
    }
    panic!("bounded actual callback request sequence did not finish");
}

async fn legacy_callback_expired_deadline_trial(annotated: bool, owner_switch: bool) -> (LogicalTime, LogicalTime) {
    use reverie::Tool;
    use crate::preemptions::callback_history::{Origin, Site, Startup};
    use crate::slice_boundary::callbacks::Callback;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("expired-deadline.json");
    let tid = DetTid::from_raw(17);
    let peer = DetTid::from_raw(18);
    let mut config = Config {
        sequentialize_threads: true,
        replay_schedule_from: Some(path.clone()),
        max_timeslice: std::num::NonZeroU64::new(5_000_000),
        target_timeslice: std::num::NonZeroU64::new(1_000_000),
        ..Config::default()
    }.with_backend(|backend| backend.delivers_preemption_timer_events = true);
    config.validate();
    let start = crate::ThreadState::new(tid, &config, ()).thread_logical_time.as_nanos();
    let at = start + Duration::from_nanos(110);
    let mut writer = crate::preemptions::PreemptionWriter::new(None);
    writer.register_thread(tid, DEFAULT_PRIORITY);
    // The first actual branch installs the next Branch(10) deadline. At that
    // deadline, the following Branch/Other/Syscall window supplies no new one.
    writer.insert_schedevent(SchedEvent::branches(tid, 1));
    writer.insert_schedevent(SchedEvent::branches(tid, 10));
    let peer_event = SchedEvent::syscall(peer, Sysno::getpid, detcore_model::schedule::SyscallPhase::Posthook);
    if owner_switch { writer.register_thread(peer, DEFAULT_PRIORITY); writer.insert_schedevent(peer_event.clone()); }
    writer.insert_schedevent(SchedEvent::branches(tid, 90));
    writer.insert_schedevent(SchedEvent { dettid: tid, op: Op::OtherInstructions, count: 1, start_rip: None, end_rip: None, end_time: None });
    writer.insert_schedevent(SchedEvent::syscall(tid, Sysno::close, detcore_model::schedule::SyscallPhase::Prehook));
    writer.insert_reprioritization(tid, at, 11, DEFAULT_PRIORITY, DEFAULT_PRIORITY);
    if annotated { writer.insert_callback_checkpoint(tid, Origin { callback: Callback::PmuTimer, site: Site::Prehook, progress: 11, last_op: Some(Op::Branch), time: at, rcbs: 11 }); }
    std::fs::write(&path, writer.into_string()).unwrap();
    let (config, state, tid, process) = cancellation_test_state_with_config(config);
    install_test_registration(&state, tid, Ivar::new());
    state.past_first_execve.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut thread = crate::ThreadState::new(tid, &config, ());
    thread.detpid = Some(process); thread.past_global_first_execve = true;
    thread.next_timeslice(&config);
    if annotated {
    thread.callback_history.install(Startup { checkpoints: state.sched.lock().unwrap().callback_checkpoints.get(&tid).map(|points| points.iter().cloned().collect()).unwrap_or_default(), ..Startup::default() });
    }
    let mut guest = LegacyDeadlineGuest { inner: ExternalRegistrationGuest { global: &state, config: &config, thread, requests: Mutex::new(Vec::new()), post_exec: false, ppid: None, private_memory: None }, clock_reads: 0, reported_rcbs: 1, timers: Vec::new(), responses: Mutex::new(Vec::new()) };
    let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
    let first = tool.pre_handler_hook_observed(&mut guest, true, crate::CheckIn::Event, Some(Callback::PmuTimer)).await;
    assert_eq!(guest.thread.end_of_timeslice, Some(at));
    assert_eq!(guest.thread.max_timeslice_end, Some(at));
    tool.post_handler_hook_observed(&mut guest, crate::CheckIn::Event, first).await;
    assert!(matches!(guest.timers.last(), Some(reverie::TimerSchedule::Rcbs(10))));
    guest.reported_rcbs = 11;
    let mut previous = Err(crate::scheduler::SkipTurn);
    let (token, pre_commits, pre_skips) = {
        let mut pre = Box::pin(tool.pre_handler_hook_observed(&mut guest, true, crate::CheckIn::Event, Some(Callback::PmuTimer)));
        if owner_switch {
            state.sched.lock().unwrap().thread_tree.add_child(tid, peer, false);
            install_test_registration(&state, peer, Ivar::new());
            assert!(futures::poll!(&mut pre).is_pending());
            assert!(state.sched.lock().unwrap().next_turns[&tid].req.try_read().unwrap().unwrap().resources.contains_key(&ResourceID::TraceReplay));
            let mut peer_thread = crate::ThreadState::new(peer, &config, ()); peer_thread.detpid = Some(process);
            let mut peer_guest = ExternalRegistrationGuest { global: &state, config: &config, thread: peer_thread, requests: Mutex::new(Vec::new()), post_exec: false, ppid: None, private_memory: None };
            let mut peer_request = Box::pin(super::resource_request(&mut peer_guest, Resources::new(peer)));
            assert!(futures::poll!(&mut peer_request).is_pending());
            let granted = crate::scheduler::do_a_turn_blocking(state.sched.clone(), state.global_time.clone(), &previous).await.unwrap();
            assert_eq!(granted.tid, peer); peer_request.await;
            peer_guest.thread.thread_logical_time.add_syscall();
            let mut observed_peer = peer_event; observed_peer.end_time = Some(peer_guest.thread.thread_logical_time.as_nanos());
            let mut peer_trace = Box::pin(super::trace_schedevent(&mut peer_guest, observed_peer, false));
            assert!(futures::poll!(&mut peer_trace).is_pending());
            previous = Ok(granted);
            let resumed = crate::scheduler::do_a_turn_blocking(state.sched.clone(), state.global_time.clone(), &previous).await.unwrap();
            assert_eq!(resumed.tid, tid); assert!(resumed.resources.contains_key(&ResourceID::TraceReplay));
            previous = Ok(resumed);
            drop(peer_trace);
        }
        drive_callback_hook(&state, tid, pre, &mut previous).await
    };
    let (_, post_commits, post_skips) = drive_callback_hook(&state, tid, tool.post_handler_hook_observed(&mut guest, crate::CheckIn::Event, token), &mut previous).await;
    assert_eq!(guest.thread.thread_logical_time.as_nanos(), at);
    assert_eq!(pre_commits + post_commits, 1, "one original expiry commit; annotated={annotated}, owner_switch={owner_switch}, pre={pre_commits}, post={post_commits}");
    assert_eq!(pre_skips + post_skips, usize::from(annotated));
    assert_eq!(guest.thread.end_of_timeslice, Some(at + Duration::from_nanos(5_000_000)));
    assert_eq!(guest.thread.max_timeslice_end, guest.thread.end_of_timeslice);
    assert!(matches!(guest.timers.last(), Some(reverie::TimerSchedule::Rcbs(500_000))));
    let (_, tail_commits, tail_skips) = drive_callback_hook(&state, tid, super::resource_request(&mut guest, Resources::new(tid)), &mut previous).await;
    assert_eq!((tail_commits, tail_skips), (1, 0));
    let global = state.global_time.lock().unwrap().as_nanos();
    let scheduler = state.sched.lock().unwrap();
    let replayer = scheduler.replayer.as_ref().unwrap();
    assert!(replayer.desync_counts.values().all(|stats| stats.soft == 0 && stats.hard == 0));
    assert_eq!((replayer.cursor.peek().unwrap().op, replayer.cursor.peek().unwrap().count), (Op::Branch, 90));
    assert!(scheduler.callback_checkpoints.values().all(|points| points.is_empty()));
    (global, guest.thread.end_of_timeslice.unwrap())
}

#[tokio::test]
async fn legacy_callback_checkpoint_subsumes_expired_continue_deadline_once() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let baseline = legacy_callback_expired_deadline_trial(false, false).await;
        assert_eq!(legacy_callback_expired_deadline_trial(true, false).await, baseline);
    }).await.expect("bounded expired Continue(None) callback");
}

#[tokio::test]
async fn legacy_callback_checkpoint_subsumes_expired_tracereplay_deadline_once() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let baseline = legacy_callback_expired_deadline_trial(false, true).await;
        assert_eq!(legacy_callback_expired_deadline_trial(true, true).await, baseline);
    }).await.expect("bounded expired TraceReplay Go(None) callback");
}
