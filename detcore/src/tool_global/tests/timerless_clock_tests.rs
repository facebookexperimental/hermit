/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The shared accounting cut must not depend on the ability to arm a timer.
//! The RPC stub acknowledges schedule records without changing time or a
//! deadline. This tests local accounting/record submission, not grant authority.

use std::sync::Arc;
use std::sync::Mutex;

use clap::Parser;
use detcore_model::schedule::Op;
use detcore_model::schedule::SchedEvent;
use reverie::Errno;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Tid;
use reverie::Tool;

use super::ExternalRegistrationStack;
use crate::CheckIn;
use crate::Config;
use crate::Detcore;
use crate::GlobalState;
use crate::ThreadState;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::tool_global::TraceSchedEventResponse;
use crate::types::DetTid;
use crate::types::LogicalTime;

const TID: DetTid = DetTid::from_raw(17);
const RIP: u64 = 0x401000;

struct ClockGuest {
    config: Config,
    thread: ThreadState<()>,
    raw: u64,
    reads: usize,
    records: Mutex<Vec<SchedEvent>>,
    precise_timers: Vec<u64>,
    imprecise_timers: Vec<u64>,
    rpc_state: Option<GlobalState>,
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for ClockGuest {
    async fn send_rpc(
        &self,
        request: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let (time, mm, request) = request;
        assert_eq!(mm, self.thread.mm_id);
        // DetTime's PartialEq compares only nanoseconds: compare all fields.
        assert_eq!(
            serde_json::to_value(&time).unwrap(),
            serde_json::to_value(&self.thread.thread_logical_time).unwrap(),
        );
        match request {
            GlobalRequest::TraceSchedEvent(event, pid, bootstrap) => {
                assert_eq!(pid, TID);
                self.records.lock().unwrap().push(event.clone());
                if let Some(state) = &self.rpc_state {
                    return state
                        .receive_rpc(
                            self.tid(),
                            (
                                time,
                                mm,
                                GlobalRequest::TraceSchedEvent(event, pid, bootstrap),
                            ),
                        )
                        .await;
                }
                (
                    None,
                    GlobalResponse::TraceSchedEvent(TraceSchedEventResponse {
                        print_stack_strace: None,
                        timeslice: None,
                    }),
                )
            }
            other => panic!("clock observation must not yield or issue another RPC: {other:?}"),
        }
    }

    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for ClockGuest {
    type Memory = reverie::syscalls::LocalMemory;
    type Stack = ExternalRegistrationStack;

    fn tid(&self) -> Tid {
        Tid::from_raw(TID.as_raw())
    }
    fn pid(&self) -> Tid {
        self.tid()
    }
    fn ppid(&self) -> Option<Tid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        reverie::syscalls::LocalMemory::new()
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        // SAFETY: user_regs_struct contains only integer register fields.
        let mut registers: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        registers.rip = RIP;
        registers
    }
    async fn stack(&mut self) -> Self::Stack {
        panic!("clock observation must not use a guest stack")
    }
    async fn daemonize(&mut self) {
        panic!("clock observation must not daemonize")
    }
    async fn inject<S: reverie::syscalls::SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
        panic!("clock observation must not inject a syscall")
    }
    async fn tail_inject<S: reverie::syscalls::SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("clock observation must not tail-inject a syscall")
    }
    fn set_timer(&mut self, schedule: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        assert!(
            self.config.backend.delivers_preemption_timer_events,
            "timerless clock observation attempted to arm an imprecise timer"
        );
        let reverie::TimerSchedule::Rcbs(rcbs) = schedule else {
            panic!("expected an RCB timer");
        };
        self.imprecise_timers.push(rcbs);
        Ok(())
    }
    fn set_timer_precise(
        &mut self,
        schedule: reverie::TimerSchedule,
    ) -> Result<(), reverie::Error> {
        assert!(
            self.config.backend.delivers_preemption_timer_events,
            "timerless clock observation attempted to arm a precise timer"
        );
        let reverie::TimerSchedule::Rcbs(rcbs) = schedule else {
            panic!("expected an RCB timer");
        };
        self.precise_timers.push(rcbs);
        Ok(())
    }
    fn read_clock(&mut self) -> Result<u64, reverie::Error> {
        self.reads += 1;
        Ok(self.raw)
    }
}

#[test]
fn rcb_accounting_progresses_without_an_implicit_preemption_timer() {
    futures::executor::block_on(async {
        let mut completed = Vec::new();
        // Use the actual Config parser: omitted maximum is Some(default),
        // while both explicit off spellings are None. Never rewrite either.
        for (name, arguments, charges, observes) in [
            (
                "disabled",
                vec!["detcore", "--max-timeslice=disabled"],
                false,
                false,
            ),
            ("zero", vec!["detcore", "--max-timeslice=0"], false, false),
            ("implicit maximum", vec!["detcore"], true, true),
            ("no RCB time", vec!["detcore", "--no-rcb-time"], false, true),
        ] {
            let mut config = Config::parse_from(arguments);
            config.sequentialize_threads = true;
            config.record_preemptions = true;
            config.backend.delivers_preemption_timer_events = false;
            assert_eq!(config.max_timeslice.is_some(), observes, "{name}");
            assert_eq!(config.use_rcb_time(), charges, "{name}");
            let tool: Detcore = Detcore::new(Tid::from_raw(TID.as_raw()), &config);
            let mut thread = tool.init_thread_state(Tid::from_raw(TID.as_raw()), None);
            thread.detpid = Some(TID);
            // This is the actual shared slice initialization, not an invented
            // deadline inserted just to satisfy the accounting assertion.
            thread.next_timeslice(&config);
            let cpu_owner = Arc::clone(&thread.process_cpu_time);
            let floor_owner = Arc::clone(&thread.guest_clock);
            let floor_before = serde_json::to_value(&*floor_owner.lock().unwrap()).unwrap();
            let initial_time = serde_json::to_value(&thread.thread_logical_time).unwrap();
            assert_eq!(
                initial_time["multiplier"],
                serde_json::json!(if charges { 1.0 } else { 500.0 }),
                "{name}: preserve the existing Config-derived cost model",
            );
            let initial_cpu = serde_json::to_value(&*cpu_owner.lock().unwrap()).unwrap();
            let mut guest = ClockGuest {
                config,
                thread,
                raw: 0,
                reads: 0,
                records: Mutex::new(Vec::new()),
                precise_timers: Vec::new(),
                imprecise_timers: Vec::new(),
                rpc_state: None,
            };
            let mut expected_records = Vec::new();
            for (index, (raw, delta)) in [(0, 0), (1, 1), (1, 0), (2, 1)].into_iter().enumerate() {
                guest.raw = raw;
                // Uses update_logical_time_rcbs and its real schedule-record
                // submission loop. Repeated input must not charge twice.
                tool.pre_handler_hook(&mut guest, false, CheckIn::Event)
                    .await;
                let charged = if charges { raw } else { 0 };
                let nanos = LogicalTime::from_nanos(charged * 10);
                let mut expected_time = initial_time.clone();
                expected_time["rcbs"] = serde_json::json!(charged);
                assert_eq!(
                    serde_json::to_value(&guest.thread.thread_logical_time).unwrap(),
                    expected_time,
                    "{name}, sample{index}"
                );
                assert_eq!(
                    guest.thread.thread_logical_time.user_cpu_time(),
                    nanos,
                    "{name}"
                );
                assert_eq!(
                    guest.thread.thread_logical_time.system_cpu_time(),
                    LogicalTime::ZERO,
                    "{name}"
                );
                assert_eq!(
                    guest.thread.committed_clock_value,
                    if observes { raw } else { 0 },
                    "{name}"
                );
                assert_eq!(guest.reads, if observes { index + 1 } else { 0 }, "{name}");
                assert_eq!(guest.thread.last_accounted_user_time, nanos, "{name}");
                assert_eq!(
                    guest.thread.last_accounted_system_time,
                    LogicalTime::ZERO,
                    "{name}"
                );
                assert!(
                    Arc::ptr_eq(&cpu_owner, &guest.thread.process_cpu_time),
                    "{name}"
                );
                let mut expected_cpu = initial_cpu.clone();
                expected_cpu["snapshot"]["user"] = serde_json::to_value(nanos).unwrap();
                assert_eq!(
                    serde_json::to_value(&*cpu_owner.lock().unwrap()).unwrap(),
                    expected_cpu,
                    "{name}"
                );
                assert!(
                    Arc::ptr_eq(&floor_owner, &guest.thread.guest_clock),
                    "{name}"
                );
                assert_eq!(
                    serde_json::to_value(&*floor_owner.lock().unwrap()).unwrap(),
                    floor_before,
                    "{name}"
                );
                if charges {
                    if delta != 0 {
                        expected_records.push(SchedEvent {
                            dettid: TID,
                            op: Op::Branch,
                            count: delta,
                            start_rip: None,
                            end_rip: None,
                            end_time: Some(nanos),
                        });
                    }
                    // Imprecise cuts record OtherInstructions even when the
                    // counter is unchanged. Four samples produce four records.
                    expected_records.push(SchedEvent {
                        dettid: TID,
                        op: Op::OtherInstructions,
                        count: 1,
                        start_rip: None,
                        end_rip: std::num::NonZeroUsize::new(RIP as usize),
                        end_time: Some(nanos),
                    });
                }
                assert_eq!(
                    *guest.records.lock().unwrap(),
                    expected_records,
                    "{name}, sample{index}"
                );
            }
            completed.push((name, tool, guest));
        }
        // Complete every accounting/record trajectory before the timer check.
        // At the unmodified H92 base the implicit row reaches the precise
        // setter's panic. This must be a behavioral RED, not a compile error.
        for (name, tool, mut guest) in completed {
            tool.post_handler_hook(&mut guest, CheckIn::Event).await;
            let expected_maximum = guest.config.max_timeslice.map(|maximum| {
                crate::types::DetTime::new(&guest.config).as_nanos()
                    + LogicalTime::from_nanos(maximum.get())
            });
            assert_eq!(guest.thread.max_timeslice_end, expected_maximum, "{name}");
            assert_eq!(guest.thread.last_rcb_timer, None, "{name}");
            assert!(!guest.thread.last_rcb_timer_is_max, "{name}");
        }
    });
}

#[test]
fn interleaved_clock_observations_preserve_deadlines_and_timer_capability() {
    futures::executor::block_on(async {
        for (delivers_timer, no_rcb_time) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut config = Config::parse_from(["detcore"]);
            config.sequentialize_threads = true;
            config.record_preemptions = true;
            config.backend.delivers_preemption_timer_events = delivers_timer;
            config.no_rcb_time = no_rcb_time;
            assert_eq!(config.max_timeslice.unwrap().get(), 200_000_000);
            assert_eq!(config.use_rcb_time(), !no_rcb_time);
            let tool: Detcore = Detcore::new(Tid::from_raw(TID.as_raw()), &config);
            let mut thread = tool.init_thread_state(Tid::from_raw(TID.as_raw()), None);
            thread.detpid = Some(TID);
            let initial_clock = thread.thread_logical_time.as_nanos();
            thread.next_timeslice(&config);
            let deadline = initial_clock + LogicalTime::from_nanos(200_000_000);
            assert_eq!(thread.end_of_timeslice, Some(deadline));
            assert_eq!(thread.max_timeslice_end, Some(deadline));
            let cpu_owner = Arc::clone(&thread.process_cpu_time);
            let floor_owner = Arc::clone(&thread.guest_clock);
            let initial_time = serde_json::to_value(&thread.thread_logical_time).unwrap();
            let initial_cpu = serde_json::to_value(&*cpu_owner.lock().unwrap()).unwrap();
            let initial_floor = serde_json::to_value(&*floor_owner.lock().unwrap()).unwrap();
            let mut guest = ClockGuest {
                config: config.clone(),
                thread,
                raw: 0,
                reads: 0,
                records: Mutex::new(Vec::new()),
                precise_timers: Vec::new(),
                imprecise_timers: Vec::new(),
                rpc_state: None,
            };
            let mut expected_records = Vec::new();
            let mut expected_timers = Vec::new();
            for (index, (raw, delta, remaining)) in [
                (0, 0, 20_000_000),
                (1, 1, 19_999_999),
                (1, 0, 19_999_999),
                (2, 1, 19_999_998),
            ]
            .into_iter()
            .enumerate()
            {
                guest.raw = raw;
                tool.pre_handler_hook(&mut guest, false, CheckIn::Event)
                    .await;
                let charged = if no_rcb_time { 0 } else { raw };
                let remaining = if no_rcb_time { 20_000_000 } else { remaining };
                let nanos = LogicalTime::from_nanos(charged * 10);
                let mut expected_time = initial_time.clone();
                expected_time["rcbs"] = serde_json::json!(charged);
                assert_eq!(
                    serde_json::to_value(&guest.thread.thread_logical_time).unwrap(),
                    expected_time
                );
                assert_eq!(guest.thread.committed_clock_value, raw);
                assert_eq!(guest.reads, index + 1);
                assert_eq!(guest.thread.last_accounted_user_time, nanos);
                assert_eq!(guest.thread.last_accounted_system_time, LogicalTime::ZERO);
                assert!(Arc::ptr_eq(&cpu_owner, &guest.thread.process_cpu_time));
                assert!(Arc::ptr_eq(&floor_owner, &guest.thread.guest_clock));
                let mut expected_cpu = initial_cpu.clone();
                expected_cpu["snapshot"]["user"] = serde_json::to_value(nanos).unwrap();
                assert_eq!(
                    serde_json::to_value(&*cpu_owner.lock().unwrap()).unwrap(),
                    expected_cpu
                );
                assert_eq!(
                    serde_json::to_value(&*floor_owner.lock().unwrap()).unwrap(),
                    initial_floor
                );
                if !no_rcb_time {
                    if delta != 0 {
                        expected_records.push(SchedEvent {
                            dettid: TID,
                            op: Op::Branch,
                            count: delta,
                            start_rip: None,
                            end_rip: None,
                            end_time: Some(nanos),
                        });
                    }
                    expected_records.push(SchedEvent {
                        dettid: TID,
                        op: Op::OtherInstructions,
                        count: 1,
                        start_rip: None,
                        end_rip: std::num::NonZeroUsize::new(RIP as usize),
                        end_time: Some(nanos),
                    });
                }
                assert_eq!(*guest.records.lock().unwrap(), expected_records);
                assert_eq!(guest.thread.end_of_timeslice, Some(deadline));
                assert_eq!(guest.thread.max_timeslice_end, Some(deadline));

                // A post-hook may arm the real timer but cannot reset the
                // logical deadline or mutate accounting, ownership or records.
                let mut expected_post = guest.thread.clone();
                expected_post.last_rcb_timer = delivers_timer.then_some(remaining);
                expected_post.last_rcb_timer_is_max = delivers_timer;
                let expected_post =
                    bincode::serde::encode_to_vec(&expected_post, bincode::config::legacy())
                        .unwrap();
                tool.post_handler_hook(&mut guest, CheckIn::Event).await;
                assert_eq!(
                    bincode::serde::encode_to_vec(&guest.thread, bincode::config::legacy())
                        .unwrap(),
                    expected_post,
                );
                assert!(Arc::ptr_eq(&cpu_owner, &guest.thread.process_cpu_time));
                assert!(Arc::ptr_eq(&floor_owner, &guest.thread.guest_clock));
                // Serde omits runtime-only publications and projections. This
                // root-only fixture owns none; check them separately.
                assert!(guest.thread.parent_cpu_publication.is_none());
                assert!(guest.thread.published_runtime_private_memory.is_none());
                assert!(guest.thread.restart_block.is_none());
                assert!(guest.thread.signal_register_projection.is_none());
                assert_eq!(guest.reads, index + 1);
                assert_eq!(*guest.records.lock().unwrap(), expected_records);
                assert_eq!(guest.thread.end_of_timeslice, Some(deadline));
                assert_eq!(guest.thread.max_timeslice_end, Some(deadline));
                if delivers_timer {
                    expected_timers.push(remaining);
                }
                assert_eq!(guest.precise_timers, expected_timers);
                assert!(guest.imprecise_timers.is_empty());
            }

            // The maximum remains a logical bound even without timer delivery.
            // A larger target must not escape that configured ceiling.
            config.target_timeslice = std::num::NonZeroU64::new(400_000_000);
            let mut capped = tool.init_thread_state(Tid::from_raw(TID.as_raw()), None);
            capped.next_timeslice(&config);
            assert_eq!(capped.end_of_timeslice, Some(deadline));
            assert_eq!(capped.max_timeslice_end, Some(deadline));

            // Timer delivery must not alter when an instruction trap yields.
            // The ordinary target expires at 100 RCBs, while syscall-only
            // instruction traps keep the configured maximum of 2,000 RCBs.
            let mut boundary_config = config.clone();
            boundary_config.no_rcb_time = false;
            boundary_config.target_timeslice = std::num::NonZeroU64::new(1_000);
            boundary_config.max_timeslice = std::num::NonZeroU64::new(20_000);
            boundary_config.target_timeslice_syscalls_only = true;
            let mut boundary = ThreadState::new(TID, &boundary_config, ());
            let initial = boundary.thread_logical_time.as_nanos();
            boundary.next_timeslice(&boundary_config);
            for (delta, total, target_expired, trap_ends) in [
                (99, 99, false, false),
                (1, 100, true, false),
                (1_899, 1_999, true, false),
                (1, 2_000, true, true),
            ] {
                boundary.thread_logical_time.add_rcbs(delta);
                assert_eq!(
                    boundary.thread_logical_time.as_nanos(),
                    initial + LogicalTime::from_nanos(total * 10),
                );
                assert_eq!(
                    boundary.end_of_timeslice,
                    Some(initial + LogicalTime::from_nanos(1_000)),
                );
                assert_eq!(
                    boundary.max_timeslice_end,
                    Some(initial + LogicalTime::from_nanos(20_000)),
                );
                assert_eq!(boundary.timeslice_expired(), target_expired);
                assert_eq!(
                    boundary.instruction_trap_ends_timeslice(&boundary_config),
                    trap_ends,
                );
                assert_eq!(boundary.last_rcb_timer, None);
                assert!(!boundary.last_rcb_timer_is_max);
            }
        }
    });
}

#[test]
fn dropped_signal_rearms_only_an_available_timer_without_accounting() {
    for delivers_timer in [false, true] {
        for imprecise in [false, true] {
            let mut config = Config::parse_from(["detcore"]);
            config.sequentialize_threads = true;
            config.backend.delivers_preemption_timer_events = delivers_timer;
            config.imprecise_timers = imprecise;
            let tool: Detcore = Detcore::new(Tid::from_raw(TID.as_raw()), &config);
            let mut thread = tool.init_thread_state(Tid::from_raw(TID.as_raw()), None);
            thread.detpid = Some(TID);
            thread.next_timeslice(&config);
            thread.committed_clock_value = 1_000;
            // In the timerless row this is deliberately synthetic stale state:
            // it makes the capability guard observable, not an actual arm.
            thread.last_rcb_timer = Some(500);
            let before = bincode::serde::encode_to_vec(&thread, bincode::config::legacy()).unwrap();
            let cpu = Arc::clone(&thread.process_cpu_time);
            let floor = Arc::clone(&thread.guest_clock);
            let mut guest = ClockGuest {
                config,
                thread,
                raw: 1_200,
                reads: 0,
                records: Mutex::new(Vec::new()),
                precise_timers: Vec::new(),
                imprecise_timers: Vec::new(),
                rpc_state: None,
            };
            tool.keep_timer_across_dropped_signal(&mut guest);
            assert_eq!(guest.reads, usize::from(delivers_timer));
            assert_eq!(
                guest.precise_timers,
                if delivers_timer && !imprecise {
                    vec![300]
                } else {
                    vec![]
                }
            );
            assert_eq!(
                guest.imprecise_timers,
                if delivers_timer && imprecise {
                    vec![300]
                } else {
                    vec![]
                }
            );
            assert_eq!(
                bincode::serde::encode_to_vec(&guest.thread, bincode::config::legacy()).unwrap(),
                before,
            );
            assert!(Arc::ptr_eq(&cpu, &guest.thread.process_cpu_time));
            assert!(Arc::ptr_eq(&floor, &guest.thread.guest_clock));
            assert!(guest.thread.parent_cpu_publication.is_none());
            assert!(guest.records.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn replay_continue_installs_a_logical_deadline_without_requiring_a_timer() {
    for delivers_timer in [false, true] {
        let sample_time =
            crate::types::DetTime::new(&Config::default()).as_nanos() + LogicalTime::from_nanos(70);
        let event = SchedEvent {
            dettid: TID,
            op: Op::OtherInstructions,
            count: 1,
            start_rip: None,
            end_rip: std::num::NonZeroUsize::new(RIP as usize),
            end_time: Some(sample_time),
        };
        let branch = SchedEvent {
            op: Op::Branch,
            count: 123,
            end_rip: None,
            end_time: Some(sample_time + LogicalTime::from_nanos(1_230)),
            ..event.clone()
        };
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            crate::preemptions::PreemptionRecord::from_sched_events(vec![
                event.clone(),
                branch.clone(),
                SchedEvent {
                    end_time: branch.end_time,
                    ..event.clone()
                },
                SchedEvent {
                    count: 1,
                    end_time: Some(sample_time + LogicalTime::from_nanos(1_240)),
                    ..branch.clone()
                },
            ])
            .to_string(),
        )
        .unwrap();
        let config = Config {
            sequentialize_threads: true,
            replay_schedule_from: Some(file.path().to_path_buf()),
            ..Config::default()
        }
        .with_backend(|backend| backend.delivers_preemption_timer_events = delivers_timer);
        let state = GlobalState::initialize(&config, false);
        state
            .past_first_execve
            .store(true, std::sync::atomic::Ordering::SeqCst);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(TID, TID, true);
        super::install_test_registration(&state, TID, crate::ivar::Ivar::new());
        let tool: Detcore = Detcore::new(Tid::from_raw(TID.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(TID.as_raw()), None);
        thread.detpid = Some(TID);
        thread.past_global_first_execve = true;
        thread.thread_logical_time.add_rcbs(7);
        thread.committed_clock_value = 7;
        thread.account_process_cpu_time();
        let now = thread.thread_logical_time.as_nanos();
        assert_eq!(now, sample_time);
        let deadline = now + LogicalTime::from_nanos(1_230);
        let cpu = Arc::clone(&thread.process_cpu_time);
        let floor = Arc::clone(&thread.guest_clock);
        let mut expected = thread.clone();
        expected.end_of_timeslice = Some(deadline);
        expected.max_timeslice_end = Some(deadline);
        let expected = bincode::serde::encode_to_vec(&expected, bincode::config::legacy()).unwrap();
        let mut guest = ClockGuest {
            config,
            thread,
            raw: 7,
            reads: 0,
            records: Mutex::new(Vec::new()),
            precise_timers: Vec::new(),
            imprecise_timers: Vec::new(),
            rpc_state: Some(state),
        };
        // This invokes the actual receive_rpc/recv_trace_schedevent/replayer
        // path. Continue keeps the current turn; no grant is fabricated.
        super::super::trace_schedevent(&mut guest, event.clone(), false).await;
        assert_eq!(*guest.records.lock().unwrap(), vec![event]);
        assert_eq!(
            bincode::serde::encode_to_vec(&guest.thread, bincode::config::legacy()).unwrap(),
            expected,
        );
        assert!(Arc::ptr_eq(&cpu, &guest.thread.process_cpu_time));
        assert!(Arc::ptr_eq(&floor, &guest.thread.guest_clock));
        assert!(guest.thread.parent_cpu_publication.is_none());
        assert_eq!(guest.reads, 0);
        assert!(guest.precise_timers.is_empty());
        assert!(guest.imprecise_timers.is_empty());
        let sched = guest.rpc_state.as_ref().unwrap().sched.lock().unwrap();
        let replayer = sched.replayer.as_ref().unwrap();
        assert_eq!(replayer.traced_event_count, 1);
        assert_eq!(replayer.events_popped, 1);
        assert_eq!(replayer.cursor.peek(), Some(&branch));
        assert_eq!(sched.turn, 0);
    }
}
