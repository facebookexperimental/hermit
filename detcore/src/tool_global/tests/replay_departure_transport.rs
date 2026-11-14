/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Production RPC/cleanup controls with a modeled guest and actual startup.

use std::sync::Mutex;
use std::time::Duration;

use reverie::GlobalTool;
use reverie::Tid;

use super::ExternalRegistrationGuest;
use crate::Config;
use crate::GlobalState;
use crate::ThreadState;
use crate::preemptions::PreemptionRecord;
use crate::preemptions::ThreadHistory;
use crate::preemptions::boundary_recording::Recording;
use crate::scheduler::DEFAULT_PRIORITY;
use crate::tool_global::GlobalRequest as Q;
use crate::tool_global::GlobalResponse as R;
use crate::tool_global::ThreadDeregistration;
use crate::tool_global::replay_departure::Refusal;
use crate::types::DetTid;
use crate::types::LogicalTime;

async fn fixture(edited: bool) -> (tempfile::TempDir, Config, GlobalState, ThreadState<()>) {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.json");
    let config = crate::clock_identity::test_sources::qualified(Config {
        sequentialize_threads: true,
        max_timeslice: None,
        target_timeslice: std::num::NonZeroU64::new(100_000),
        panic_on_rcb_overshoot: false,
        replay_preemptions_from: Some(input.clone()),
        replay_edited_preemptions: edited,
        record_preemptions: true,
        record_preemptions_to: Some(dir.path().join("output.json")),
        ..Config::default()
    });
    let tid = DetTid::from_raw(3);
    let mut thread = ThreadState::new(tid, &config, ());
    thread.detpid = Some(tid);
    let epoch = thread.thread_logical_time.as_nanos();
    let history = ThreadHistory::new()
        .with_prio_changes(vec![
            (epoch + Duration::from_nanos(1_000_000), DEFAULT_PRIORITY),
            (
                epoch + Duration::from_nanos(2_000_000),
                DEFAULT_PRIORITY + 1,
            ),
        ])
        .with_preemption_rcbs(vec![1, 1]);
    let record: PreemptionRecord = serde_json::from_value(serde_json::json!({
        "per_thread": {"3": history, "4": history}, "global": [],
    }))
    .unwrap();
    let record = record.with_identity(Some(crate::clock_identity::test_sources::known()));
    record.validate().unwrap();
    record.write_to_disk(&input).unwrap();
    let state = <GlobalState as GlobalTool>::init_global_state(&config).await;
    let mut guest = ExternalRegistrationGuest {
        global: &state,
        config: &config,
        thread,
        requests: Mutex::new(Vec::new()),
        post_exec: false,
        ppid: None,
        private_memory: None,
    };
    assert_eq!(
        crate::tool_global::create_child_thread(&mut guest, tid, 0, None, libc::SIGCHLD, None)
            .await,
        None
    );
    let (history, first_slice) =
        crate::tool_global::thread_start_request(&config, &mut guest, tid).await;
    assert!(history.is_some());
    crate::tool_global::finish_thread_start(&config, &mut guest, history, first_slice);
    assert!(state.sched.lock().unwrap().holds_serial_grant(tid));
    let thread = guest.thread;
    (dir, config, state, thread)
}

fn report() -> Q {
    Q::ReplayDeparture {
        previous: LogicalTime::from_nanos(101),
        observed: LogicalTime::from_nanos(100),
        slice: 7,
    }
}

async fn deregister(state: &GlobalState, thread: &ThreadState<()>) {
    assert_eq!(
        state
            .receive_rpc(
                Tid::from_raw(thread.dettid.as_raw()),
                (
                    thread.thread_logical_time.clone(),
                    thread.mm_id,
                    Q::DeregisterThread(ThreadDeregistration {
                        dettid: thread.dettid,
                        detpid: thread.detpid.unwrap(),
                        mm: thread.mm_id,
                        thread_start_entered: true,
                        passed_descriptors_out: false,
                        wait_status: 0,
                        vfork_origin: None,
                        timeslice_stats: thread.stats.timeslice_stats,
                        slice_ran_past_bound: false,
                        syscall_count: thread.stats.syscall_count,
                        syscall_histogram: (&thread.stats.syscall_histogram).into(),
                        chaos_epochs: Vec::new(),
                        killed_by_group_signal: false,
                    }),
                )
            )
            .await,
        (None, R::DeregisterThread(()))
    );
}

fn bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
    bincode::serde::encode_to_vec(value, bincode::config::legacy()).unwrap()
}

async fn neutral(state: &GlobalState, thread: &ThreadState<()>, request: Q, direct: bool) -> R {
    neutral_from(state, thread, request, direct, thread.dettid).await
}

async fn neutral_from(
    state: &GlobalState,
    thread: &ThreadState<()>,
    request: Q,
    direct: bool,
    sender: DetTid,
) -> R {
    let now = state.global_time.lock().unwrap().as_nanos();
    {
        let mut scheduler = state.sched.lock().unwrap();
        scheduler.register_alarm(
            thread.dettid,
            thread.dettid,
            now,
            LogicalTime::from_nanos(5000),
            LogicalTime::ZERO,
            nix::sys::signal::Signal::SIGALRM,
        );
        scheduler.note_arming_hint(thread.dettid, now);
    }
    let clock = bytes(&*state.global_time.lock().unwrap());
    let scheduler = state
        .sched
        .lock()
        .unwrap()
        .replay_observation_state_for_test();
    let pending = state.pending_exec_states.lock().unwrap().clone();
    let mut time = thread.thread_logical_time.clone();
    time.add_syscall_with_cost(987654);
    let from = Tid::from_raw(sender.as_raw());
    let request = (time, thread.mm_id, request);
    let (time, response) = if direct {
        state.dispatch_rpc(from, request).await
    } else {
        state.receive_rpc(from, request).await
    };
    assert_eq!(time, None);
    assert_eq!(bytes(&*state.global_time.lock().unwrap()), clock);
    assert_eq!(
        state
            .sched
            .lock()
            .unwrap()
            .replay_observation_state_for_test(),
        scheduler
    );
    assert_eq!(*state.pending_exec_states.lock().unwrap(), pending);
    assert_eq!(
        state
            .sched
            .lock()
            .unwrap()
            .take_deadline_hint(thread.dettid),
        Some(Duration::from_nanos(5000))
    );
    response
}

#[tokio::test]
async fn accepted_reports_preserve_turn_clock_deadline_and_keep_first_finding() {
    for direct in [false, true] {
        let (dir, _config, state, thread) = fixture(false).await;
        let event = state.sched.lock().unwrap().recorded_event_count;
        assert_eq!(
            neutral(&state, &thread, report(), direct).await,
            R::ReplayDeparture(Ok(()))
        );
        let first = state
            .sched
            .lock()
            .unwrap()
            .replay_departure()
            .cloned()
            .unwrap();
        assert_eq!(first.count, 1);
        assert_eq!(first.first.recorded_event_count, Some(event));
        assert_eq!(first.first.thread, thread.dettid);
        assert_eq!(first.first.slice, Some(7));
        assert_eq!(
            neutral(
                &state,
                &thread,
                Q::ReplayDeparture {
                    previous: LogicalTime::from_nanos(77),
                    observed: LogicalTime::from_nanos(77),
                    slice: 9,
                },
                direct
            )
            .await,
            R::ReplayDeparture(Ok(()))
        );
        let second = state
            .sched
            .lock()
            .unwrap()
            .replay_departure()
            .cloned()
            .unwrap();
        assert_eq!(second.count, 2);
        assert_eq!(second.first, first.first);
        assert_eq!(
            state.sched.lock().unwrap().replay_publication_refusal(),
            None
        );
        deregister(&state, &thread).await;
        let error = state.clean_up(false, &None).await.unwrap_err();
        assert_eq!(
            crate::ReplayDepartureError::in_error(&error)
                .unwrap()
                .summary(),
            Some(&second)
        );
        assert_eq!(
            Recording::read(&dir.path().join("output.json"))
                .unwrap()
                .replay_departed_at(),
            Some(event)
        );
    }
}

#[tokio::test]
async fn rejected_reports_refuse_cleanup_without_accepting_or_marking_a_finding() {
    for direct in [false, true] {
        for kind in [
            "zero",
            "nested",
            "advancing",
            "edited",
            "unowned",
            "wrong-mm",
            "unknown",
        ] {
            let (dir, _config, state, mut thread) = fixture(kind == "edited").await;
            let original_mm = thread.mm_id;
            let original_tid = thread.dettid;
            let (request, reason) = match kind {
                "zero" => (
                    Q::WithForwardedCount(0, Box::new(report())),
                    Refusal::Envelope,
                ),
                "nested" => (
                    Q::WithForwardedCount(
                        1,
                        Box::new(Q::WithForwardedCount(2, Box::new(report()))),
                    ),
                    Refusal::Envelope,
                ),
                "advancing" => (
                    Q::ReplayDeparture {
                        previous: LogicalTime::from_nanos(1),
                        observed: LogicalTime::from_nanos(2),
                        slice: 7,
                    },
                    Refusal::Advancing,
                ),
                "edited" => (report(), Refusal::Mode),
                "unowned" => {
                    state
                        .sched
                        .lock()
                        .unwrap()
                        .set_running_for_test(DetTid::from_raw(999));
                    (report(), Refusal::UnownedTurn)
                }
                "wrong-mm" => {
                    thread.mm_id = thread.mm_id.for_exec(thread.dettid);
                    (report(), Refusal::Incarnation)
                }
                "unknown" => (report(), Refusal::Sender),
                _ => unreachable!(),
            };
            let sender = if kind == "unknown" {
                DetTid::from_raw(999)
            } else {
                thread.dettid
            };
            assert_eq!(
                neutral_from(&state, &thread, request, direct, sender).await,
                R::ReplayDeparture(Err(reason)),
                "{kind}"
            );
            {
                let scheduler = state.sched.lock().unwrap();
                assert_eq!(scheduler.replay_departure(), None, "{kind}");
                assert_eq!(
                    scheduler.replay_publication_refusal(),
                    Some((sender, reason))
                );
            }
            thread.mm_id = original_mm;
            thread.dettid = original_tid;
            if kind == "unowned" {
                state
                    .sched
                    .lock()
                    .unwrap()
                    .set_running_for_test(original_tid);
            }
            deregister(&state, &thread).await;
            let error = state.clean_up(false, &None).await.unwrap_err();
            assert_eq!(
                crate::ReplayDepartureError::in_error(&error)
                    .unwrap()
                    .summary(),
                None
            );
            assert!(format!("{error:#}").contains(&format!("({reason:?})")));
            assert_eq!(
                Recording::read(&dir.path().join("output.json"))
                    .unwrap()
                    .replay_departed_at(),
                None
            );
        }
    }
}

#[test]
fn complete_legacy_codec_preserves_bare_counted_findings_and_neutral_acknowledgments() {
    let config = Config::default();
    let thread: ThreadState<()> = ThreadState::new(DetTid::from_raw(7), &config, ());
    for request in [report(), Q::WithForwardedCount(17, Box::new(report()))] {
        let actual = (thread.thread_logical_time.clone(), thread.mm_id, request);
        let encoded = bytes(&actual);
        let (decoded, used): (<GlobalState as GlobalTool>::Request, usize) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::legacy()).unwrap();
        assert_eq!(decoded, actual);
        assert_eq!(used, encoded.len());
    }
    for response in [
        R::ReplayDeparture(Ok(())),
        R::ReplayDeparture(Err(Refusal::UnownedTurn)),
    ] {
        let actual: <GlobalState as GlobalTool>::Response = (None, response);
        let encoded = bytes(&actual);
        let (decoded, used): (<GlobalState as GlobalTool>::Response, usize) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::legacy()).unwrap();
        assert_eq!(decoded, actual);
        assert_eq!(used, encoded.len());
    }
}

#[tokio::test]
async fn a_fresh_registered_child_without_exec_history_can_publish_in_its_grant() {
    let (dir, config, state, root_thread) = fixture(false).await;
    let child = DetTid::from_raw(4);
    let mut root = ExternalRegistrationGuest {
        global: &state,
        config: &config,
        thread: root_thread,
        requests: Mutex::new(Vec::new()),
        post_exec: false,
        ppid: None,
        private_memory: None,
    };
    let mut thread = ThreadState::new(child, &config, ());
    thread.detpid = Some(child);
    let mut guest = ExternalRegistrationGuest {
        global: &state,
        config: &config,
        thread,
        requests: Mutex::new(Vec::new()),
        post_exec: false,
        ppid: None,
        private_memory: None,
    };
    // ParentContinue may grant the new child before returning to its parent.
    // Poll both real lifecycle participants and join their natural deregistrations.
    let parent = async {
        assert_eq!(
            crate::tool_global::create_child_thread(
                &mut root,
                child,
                0,
                Some(reverie::syscalls::CloneFlags::empty()),
                libc::SIGCHLD,
                None,
            )
            .await,
            None
        );
        deregister(&state, &root.thread).await;
    };
    let child_run = async {
        let (history, first_slice) =
            crate::tool_global::thread_start_request(&config, &mut guest, child).await;
        assert!(history.is_some());
        crate::tool_global::finish_thread_start(&config, &mut guest, history, first_slice);
        {
            let scheduler = state.sched.lock().unwrap();
            assert!(scheduler.thread_was_registered(child));
            assert!(scheduler.thread_is_live(child));
            assert!(scheduler.holds_serial_grant(child));
            assert_eq!(scheduler.thread_mm(child), Some(guest.thread.mm_id));
            // No exec transition yet: the exact birth MM, not an invented exec
            // entry, authenticates this ordinary registered child.
            assert!(scheduler.rpc_incarnation_matches(child, guest.thread.mm_id.for_exec(child)));
        }
        assert_eq!(
            neutral(&state, &guest.thread, report(), false).await,
            R::ReplayDeparture(Ok(()))
        );
        deregister(&state, &guest.thread).await;
    };
    tokio::join!(parent, child_run);
    drop(root);
    drop(guest);
    let error = state.clean_up(false, &None).await.unwrap_err();
    assert_eq!(
        crate::ReplayDepartureError::in_error(&error)
            .unwrap()
            .summary()
            .unwrap()
            .first
            .thread,
        child
    );
    assert!(
        Recording::read(&dir.path().join("output.json"))
            .unwrap()
            .replay_departed_at()
            .is_some()
    );
}

#[tokio::test]
async fn terminal_lifecycle_wins_over_malformed_reports_without_a_refusal_latch() {
    use super::cancellation_test_state_with_config;
    use super::install_test_registration;
    use crate::ivar::Ivar;
    for direct in [false, true] {
        for cause in ["killed", "stale", "retired", "backend", "sigkill"] {
            for nested in [false, true] {
                let config = Config {
                    sequentialize_threads: true,
                    ..Config::default()
                }
                .with_backend(|backend| {
                    backend.needs_killed_thread_rpc_cancellation = cause != "sigkill";
                });
                let (config, state, tid, pid) = cancellation_test_state_with_config(config);
                install_test_registration(&state, tid, Ivar::new());
                let mut thread: ThreadState<()> = ThreadState::new(tid, &config, ());
                state
                    .sched
                    .lock()
                    .unwrap()
                    .record_thread_mm(tid, thread.mm_id);
                match cause {
                    "killed" => {
                        state
                            .sched
                            .lock()
                            .unwrap()
                            .logically_kill_thread(&tid, &pid, thread.mm_id)
                    }
                    "stale" => state
                        .sched
                        .lock()
                        .unwrap()
                        .install_test_exec_incarnation(tid, thread.mm_id.for_exec(pid)),
                    "retired" => {
                        let caller = DetTid::from_raw(18);
                        state
                            .sched
                            .lock()
                            .unwrap()
                            .thread_tree
                            .add_child(tid, caller, false);
                        install_test_registration(&state, caller, Ivar::new());
                        let mut scheduler = state.sched.lock().unwrap();
                        scheduler.record_thread_mm(caller, thread.mm_id);
                        scheduler.reconnect_transferred_exec(crate::scheduler::ExecReconnect {
                            caller,
                            new_leader: tid,
                            detpid: pid,
                            pre_exec_mm: thread.mm_id,
                            post_exec_mm: thread.mm_id.for_exec(pid),
                            child_tid_addr: 0,
                            reconnect_priority: Some(DEFAULT_PRIORITY),
                        });
                        assert!(scheduler.transferred_exec_tid_requires_registration(caller));
                        thread.dettid = caller;
                    }
                    "backend" => state.report_backend_failure(reverie::BackendFailure {
                        pid: Tid::from_raw(pid.as_raw()),
                        tid: Tid::from_raw(tid.as_raw()),
                        phase: "replay publication lifecycle control",
                    }),
                    "sigkill" => {
                        let sender = DetTid::from_raw(18);
                        let mut scheduler = state.sched.lock().unwrap();
                        scheduler.thread_tree.add_child(pid, sender, true);
                        let token = scheduler.reserve_sigkill(sender, pid, 0).unwrap().token;
                        assert_eq!(scheduler.commit_sigkill(token, true).len(), 1);
                    }
                    _ => unreachable!(),
                }
                let request = if nested {
                    Q::WithForwardedCount(1, Box::new(Q::WithForwardedCount(2, Box::new(report()))))
                } else {
                    Q::WithForwardedCount(0, Box::new(report()))
                };
                let before = format!("{:?}", *state.sched.lock().unwrap());
                let clock = bytes(&*state.global_time.lock().unwrap());
                let input = (thread.thread_logical_time.clone(), thread.mm_id, request);
                let from = Tid::from_raw(thread.dettid.as_raw());
                let response = if direct {
                    state.dispatch_rpc(from, input).await
                } else {
                    state.receive_rpc(from, input).await
                };
                assert_eq!(
                    response,
                    (
                        None,
                        if matches!(cause, "backend" | "sigkill") {
                            R::Retired
                        } else {
                            R::ThreadExited
                        }
                    )
                );
                assert_eq!(format!("{:?}", *state.sched.lock().unwrap()), before);
                assert_eq!(bytes(&*state.global_time.lock().unwrap()), clock);
                assert_eq!(state.sched.lock().unwrap().replay_departure(), None);
                assert_eq!(
                    state.sched.lock().unwrap().replay_publication_refusal(),
                    None
                );
            }
        }
    }
}

struct Source {
    send: std::os::fd::OwnedFd,
    written: std::sync::Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Source {
    fn new() -> Self {
        use std::os::fd::FromRawFd;
        let mut fds = [-1; 2];
        // SAFETY: writable pair storage; successful socketpair creates owners.
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        let receive = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[0]) };
        let send = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) };
        let written = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&written);
        crate::detlog::set_forwarded_source(
            receive,
            Box::new(move |bytes| sink.lock().unwrap().push(bytes.to_vec())),
        );
        Self { send, written }
    }
    fn send(&self, tid: DetTid, index: u64, bytes: &[u8]) {
        use std::os::fd::AsRawFd;
        let record = crate::detlog::tagged_forwarded_message(tid.as_raw(), index, bytes);
        // SAFETY: valid message bytes and an owned connected message socket.
        assert_eq!(
            unsafe {
                libc::send(
                    self.send.as_raw_fd(),
                    record.as_ptr().cast(),
                    record.len(),
                    libc::MSG_NOSIGNAL,
                )
            },
            record.len() as isize
        );
    }
}

#[test]
fn forwarding_reports_preserve_actual_intervals_and_sticky_loss() {
    const CASE: &str = "HERMIT_REPLAY_FINDING_FORWARDING_CASE";
    const NAME: &str = "tool_global::tests::replay_departure_transport::forwarding_reports_preserve_actual_intervals_and_sticky_loss";
    let cases = [
        "exact",
        "bare",
        "missing",
        "extra",
        "sequence",
        "uncounted",
        "forged",
        "wrong-mm",
    ];
    if let Ok(case) = std::env::var(CASE) {
        assert!(cases.contains(&case.as_str()));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (_dir, _config, state, mut thread) = fixture(false).await;
                let source = Source::new();
                assert_eq!(crate::detlog::determinism_loss(), None);
                source.send(
                    thread.dettid,
                    if case == "sequence" { 2 } else { 1 },
                    b"first exact record\n",
                );
                if case == "extra" {
                    source.send(thread.dettid, 2, b"second exact record\n");
                }
                if case == "bare" {
                    assert_eq!(
                        neutral(&state, &thread, report(), false).await,
                        R::ReplayDeparture(Ok(()))
                    );
                    assert_eq!(
                        *source.written.lock().unwrap(),
                        vec![b"first exact record\n".to_vec()]
                    );
                }
                if matches!(case.as_str(), "forged" | "wrong-mm") {
                    let mm = thread.mm_id;
                    if case == "wrong-mm" {
                        thread.mm_id = mm.for_exec(thread.dettid);
                    }
                    let sender = if case == "forged" {
                        DetTid::from_raw(999)
                    } else {
                        thread.dettid
                    };
                    let expected = if case == "forged" {
                        Refusal::Sender
                    } else {
                        Refusal::Incarnation
                    };
                    assert_eq!(
                        neutral_from(
                            &state,
                            &thread,
                            Q::WithForwardedCount(1, Box::new(report())),
                            false,
                            sender
                        )
                        .await,
                        R::ReplayDeparture(Err(expected))
                    );
                    assert!(
                        source.written.lock().unwrap().is_empty(),
                        "rejection cannot drain the pending real record"
                    );
                    assert_eq!(state.sched.lock().unwrap().replay_departure(), None);
                    assert_eq!(crate::detlog::determinism_loss(), None);
                    thread.mm_id = mm;
                }
                let count = match case.as_str() {
                    "missing" => 2,
                    "uncounted" => crate::detlog::UNCOUNTED,
                    _ => 1,
                };
                assert_eq!(
                    neutral(
                        &state,
                        &thread,
                        Q::WithForwardedCount(count, Box::new(report())),
                        true
                    )
                    .await,
                    R::ReplayDeparture(Ok(()))
                );
                let loss = crate::detlog::determinism_loss();
                assert_eq!(
                    loss.is_some(),
                    matches!(
                        case.as_str(),
                        "missing" | "extra" | "sequence" | "uncounted"
                    )
                );
                let written = source.written.lock().unwrap().clone();
                assert_eq!(written[0], b"first exact record\n");
                if case == "extra" {
                    assert_eq!(written[1], b"second exact record\n");
                }
                if loss.is_none() {
                    assert_eq!(written.len(), 1);
                }
                // A successful later interval cannot erase an earlier loss.
                source.send(thread.dettid, 1, b"later exact record\n");
                assert_eq!(
                    neutral(
                        &state,
                        &thread,
                        Q::WithForwardedCount(1, Box::new(report())),
                        false
                    )
                    .await,
                    R::ReplayDeparture(Ok(()))
                );
                assert_eq!(crate::detlog::determinism_loss(), loss);
                assert_eq!(
                    source.written.lock().unwrap().last().unwrap(),
                    b"later exact record\n"
                );
                deregister(&state, &thread).await;
                let error = state.clean_up(false, &None).await.unwrap_err();
                assert!(crate::ReplayDepartureError::in_error(&error).is_some());
                assert_eq!(crate::detlog::determinism_loss(), loss);
            });
        return;
    }
    // Actual forwarding source and loss are process-global. Each child runs
    // this same exact maintained test identity under the enclosing test bounds;
    // no production latch is reset to isolate the cases.
    for case in cases {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CASE, case)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {:?}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("running 1 test") && stdout.contains("1 passed; 0 failed"),
            "{case}: {stdout}"
        );
    }
}

#[tokio::test]
async fn a_mutated_ack_cannot_erase_actual_delivery_or_become_an_accepted_grant() {
    use crate::tool_global::replay_departure::Acknowledgment;
    use crate::tool_global::replay_departure::acknowledgment;
    let (_dir, _config, state, thread) = fixture(false).await;
    let response = state
        .receive_rpc(
            Tid::from_raw(thread.dettid.as_raw()),
            (thread.thread_logical_time.clone(), thread.mm_id, report()),
        )
        .await;
    assert_eq!(acknowledgment(response.clone()), Acknowledgment::Accepted);
    let accepted = response.1;
    for bad in [
        (Some(LogicalTime::ZERO), accepted.clone()),
        (None, R::Granted(Box::new(accepted.clone()))),
        (
            None,
            R::WithDeadline(Duration::ZERO, Box::new(accepted.clone())),
        ),
        (None, R::WithLoneGrant(Box::new(accepted))),
        (None, R::GlobalTimeLowerBound(LogicalTime::ZERO)),
    ] {
        // The production sender uses this exact classifier before its fatal
        // refusal branch. The real delivery, not the mutated reply, owns the
        // cleanup failure tested below; no live transport failure is simulated.
        assert_eq!(
            acknowledgment(bad),
            Acknowledgment::Refused(Refusal::Acknowledgment)
        );
    }
    deregister(&state, &thread).await;
    let error = state.clean_up(false, &None).await.unwrap_err();
    assert_eq!(
        crate::ReplayDepartureError::in_error(&error)
            .unwrap()
            .summary()
            .unwrap()
            .count,
        1
    );
}

#[tokio::test]
async fn publication_sender_preserves_terminal_nonresumption_and_retirement() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use super::RetirementGuest;
    use super::cancellation_test_state_with_config;
    use super::install_test_registration;
    use crate::ivar::Ivar;
    for cause in ["killed", "stale", "backend", "sigkill"] {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        }
        .with_backend(|backend| {
            backend.needs_killed_thread_rpc_cancellation = cause != "sigkill";
        });
        let (config, state, tid, pid) = cancellation_test_state_with_config(config);
        install_test_registration(&state, tid, Ivar::new());
        let mut thread: ThreadState<()> = ThreadState::new(tid, &config, ());
        thread.detpid = Some(pid);
        state
            .sched
            .lock()
            .unwrap()
            .record_thread_mm(tid, thread.mm_id);
        match cause {
            "killed" => state
                .sched
                .lock()
                .unwrap()
                .logically_kill_thread(&tid, &pid, thread.mm_id),
            "stale" => state
                .sched
                .lock()
                .unwrap()
                .install_test_exec_incarnation(tid, thread.mm_id.for_exec(pid)),
            "backend" => state.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(pid.as_raw()),
                tid: Tid::from_raw(tid.as_raw()),
                phase: "publication sender control",
            }),
            "sigkill" => {
                let sender = DetTid::from_raw(18);
                let mut scheduler = state.sched.lock().unwrap();
                scheduler.thread_tree.add_child(pid, sender, true);
                let token = scheduler.reserve_sigkill(sender, pid, 0).unwrap().token;
                assert_eq!(scheduler.commit_sigkill(token, true).len(), 1);
            }
            _ => unreachable!(),
        }
        let mut guest = RetirementGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            retired: AtomicBool::new(false),
            hold_private_memory_publication: false,
            local_memory: false,
            runs_syscalls: false,
        };
        let clock = bytes(&guest.thread.thread_logical_time);
        {
            let mut future = std::pin::pin!(crate::tool_global::replay_departure::publish(
                &mut guest,
                LogicalTime::from_nanos(2),
                LogicalTime::from_nanos(1),
                1,
            ));
            assert!(futures::poll!(future.as_mut()).is_pending(), "{cause}");
        }
        assert_eq!(
            guest.retired.load(Ordering::SeqCst),
            matches!(cause, "killed" | "stale")
        );
        assert_eq!(bytes(&guest.thread.thread_logical_time), clock);
        assert_eq!(guest.requests.lock().unwrap().len(), 1);
        assert_eq!(state.sched.lock().unwrap().replay_departure(), None);
        assert_eq!(
            state.sched.lock().unwrap().replay_publication_refusal(),
            None
        );
    }
}
