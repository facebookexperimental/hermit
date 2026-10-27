/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod callback_history_scheduler_tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use detcore_model::futex::MmId;
    use detcore_model::schedule::SchedEvent;
    use detcore_model::schedule::SyscallPhase;
    use reverie::syscalls::Sysno;

    use super::*;
    use crate::config::Config;
    use crate::types::DetPid;
    use crate::types::DetTid;
    use crate::types::GlobalTime;
    use crate::types::LogicalTime;
    fn tid(n: i32) -> DetTid {
        DetTid::from_raw(n)
    }
    fn install(s: &mut Scheduler, tid: DetTid, priority: u64, rs: Resources) {
        s.thread_tree.add_child(tid, tid, true);
        s.record_thread_mm(tid, MmId::initial(tid));
        s.priorities.insert(tid, priority);
        s.next_turns.insert(
            tid,
            ThreadNextTurn {
                dettid: tid,
                child_tid_addr: 0,
                req: Ivar::full(Ok(rs)),
                resp: Ivar::new(),
                protocol: Default::default(),
            },
        );
        s.runqueue_push_back(tid);
    }
    fn priority(t: DetTid, _replay: bool) -> Resources {
        let mut rs = Resources::new(t);
        rs.insert(
            ResourceID::PriorityChangePoint(200, LogicalTime::ZERO, 0, Vec::new()),
            Permission::W,
        );
        rs
    }

    fn attach(s: &mut Scheduler, request: &mut Resources) {
        use crate::preemptions::callback_history::Checkpoint;
        use crate::preemptions::callback_history::Origin;
        use crate::preemptions::callback_history::Site;
        use crate::slice_boundary::callbacks::Callback;
        let origin = Origin {
            callback: Callback::Syscall(Sysno::close),
            site: Site::Posthook,
            progress: 0,
            last_op: None,
            time: LogicalTime::ZERO,
            rcbs: 0,
        };
        request.callback_checkpoint = Some(Box::new(origin.clone()));
        s.callback_checkpoints.insert(
            request.tid,
            std::collections::VecDeque::from([Checkpoint {
                history_index: 0,
                origin,
            }]),
        );
    }

    // This uses only the original legacy event cursor and actual request API.
    // No boundary/Cut protocol is enabled. Metadata is the only candidate toggle.
    // The original record-vs-replay failure remains in retained baseline evidence.
    fn legacy_after_nonleader(annotated: bool) -> (i32, u64, LogicalTime, Resources) {
        let replaying = true;
        use crate::scheduler::replayer::Replayer;
        let a = tid(3);
        let b = tid(4);
        let d = tid(5);
        let mut s = Scheduler::new(&Config::default());
        let mut d_pre = SchedEvent::syscall(d, Sysno::nanosleep, SyscallPhase::Prehook);
        let mut b_post = SchedEvent::syscall(b, Sysno::getpid, SyscallPhase::Posthook);
        let mut a_post = SchedEvent::syscall(a, Sysno::close, SyscallPhase::Posthook);
        for event in [&mut d_pre, &mut b_post, &mut a_post] {
            event.end_time = Some(LogicalTime::ZERO);
        }
        if replaying {
            s.replayer = Some(Replayer::new([d_pre.clone(), b_post.clone(), a_post]));
        }
        assert!(s.boundary_replayer.is_none());
        let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        s.bump_global_time(&global, &Err(SkipTurn));
        let start = s.committed_time;
        let mut rb = Resources::new(b);
        rb.insert(
            ResourceID::SleepUntil(start + std::time::Duration::from_nanos(1)),
            Permission::W,
        );
        install(
            &mut s,
            b,
            if replaying {
                REPLAY_DEFERRED_PRIORITY
            } else {
                10
            },
            rb.clone(),
        );
        let (selected, _, response) = s.step3_peek().unwrap();
        assert_eq!(selected, b);
        assert!(s.step4_resource_block(b, &rb, &response).is_err());
        let mut a_request = priority(a, false);
        if annotated {
            attach(&mut s, &mut a_request);
        }
        install(
            &mut s,
            a,
            if replaying {
                REPLAY_DEFERRED_PRIORITY
            } else {
                100
            },
            a_request,
        );
        install(
            &mut s,
            d,
            if replaying {
                REPLAY_FOREGROUND_PRIORITY
            } else {
                50
            },
            Resources::new(d),
        );
        let (selected, _, response) = s.step3_peek().unwrap();
        assert_eq!(selected, d);
        s.step4_resource_block(d, &Resources::new(d), &response)
            .unwrap();
        s.step5_guest_unblock(d, &Resources::new(d), &response)
            .unwrap();
        s.step6_reenquue(d, super::Requeue::Back);
        if replaying {
            assert!(
                s.consume_schedevent(&d_pre).keep_running,
                "original syscall prehook does not wait for the next owner"
            );
        }
        let mut rd = Resources::new(d);
        rd.insert(
            ResourceID::SleepUntil(start + std::time::Duration::from_nanos(5_000_000)),
            Permission::W,
        );
        let filed = s.next_turns[&d].req.clone();
        assert!(filed.is_empty());
        s.request_put(&filed, rd.clone(), &global);
        s.bump_global_time(&global, &Ok(Resources::new(d)));
        assert!(s.step2b_process_timed(&global));
        let (selected, request, response) = s.step3_peek().unwrap();
        assert_eq!(selected, b);
        let granted = request.try_read().unwrap().unwrap();
        s.step4_resource_block(b, &granted, &response).unwrap();
        s.step5_guest_unblock(b, &granted, &response).unwrap();
        s.step6_reenquue(b, super::Requeue::Back);
        if replaying {
            assert!(!s.consume_schedevent(&b_post).keep_running);
            assert_eq!(s.priorities[&a], REPLAY_FOREGROUND_PRIORITY);
        }
        s.logically_kill_thread(&b, &DetPid::from_raw(b.as_raw()), MmId::initial(b));
        s.drain_pending_run_queue_removals();
        s.bump_global_time(&global, &Ok(granted));
        assert!(!s.step2b_process_timed(&global));
        assert_eq!(s.next_turns[&d].req, filed);
        assert_eq!(filed.try_read().unwrap().unwrap(), rd);
        let (selected, _, _) = s.step3_peek().unwrap();
        (selected.as_raw(), s.turn, s.committed_time, rd)
    }

    #[test]
    fn callback_checkpoint_preserves_base_legacy_nonleader_selection_and_filed_request() {
        let baseline = legacy_after_nonleader(false);
        let annotated = legacy_after_nonleader(true);
        assert_eq!(
            baseline.0, 3,
            "the independently retained unchanged-base witness selects A, not D"
        );
        assert_eq!(
            annotated, baseline,
            "metadata must not frontload an additional selection or alter the filed park"
        );
    }

    #[test]
    fn callback_checkpoint_selected_operation_reads_current_band_after_cursor_transfer() {
        use crate::scheduler::replayer::Replayer;
        for exhausted in [false, true] {
            let a = tid(3);
            let b = tid(4);
            let mut s = Scheduler::new(&Config::default());
            let mut b_post = SchedEvent::syscall(b, Sysno::getpid, SyscallPhase::Posthook);
            let mut a_post = SchedEvent::syscall(a, Sysno::close, SyscallPhase::Posthook);
            b_post.end_time = Some(LogicalTime::ZERO);
            a_post.end_time = Some(LogicalTime::ZERO);
            s.replayer = Some(Replayer::new(if exhausted {
                vec![b_post.clone()]
            } else {
                vec![b_post.clone(), a_post]
            }));
            let mut a_request = priority(a, false);
            attach(&mut s, &mut a_request);
            install(&mut s, a, REPLAY_DEFERRED_PRIORITY, a_request);
            install(&mut s, b, REPLAY_FOREGROUND_PRIORITY, Resources::new(b));
            let original = s.next_turns[&a].req.clone();
            let response = s.next_turns[&a].resp.clone();
            assert!(response.try_read().is_none());
            // The ordinary cursor changes A's band after the request is filed.
            s.consume_schedevent(&b_post);
            let band = s.get_priority(a);
            assert_eq!(
                band,
                if exhausted {
                    crate::scheduler::runqueue::DEFAULT_PRIORITY
                } else {
                    REPLAY_FOREGROUND_PRIORITY
                }
            );
            s.logically_kill_thread(&b, &DetPid::from_raw(b.as_raw()), MmId::initial(b));
            s.drain_pending_run_queue_removals();
            let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
            s.bump_global_time(&global, &Err(SkipTurn));
            let time = s.committed_time;
            let (selected, request, selected_response) = s.step3_peek().unwrap();
            assert_eq!(selected, a);
            assert_eq!(request, original);
            assert!(crate::scheduler::runqueue::is_ordinary_priority(band));
            let turn = s.turn;
            let posted = request.try_read().unwrap().unwrap();
            assert!(
                s.step4_resource_block(a, &posted, &selected_response)
                    .is_err()
            );
            assert!(s.callback_checkpoints[&a].is_empty());
            assert_eq!(s.get_priority(a), band);
            assert_eq!(s.turn, turn + 1);
            assert!(response.try_read().is_none());
            assert_ne!(s.next_turns[&a].req, original);
            let empty = s.next_turns[&a].req.try_read().unwrap().unwrap();
            assert!(empty.resources.is_empty());
            s.bump_global_time(&global, &Err(SkipTurn));
            assert_eq!(s.committed_time, time);
            assert!(!s.step2b_process_timed(&global));
            let (selected, request, response) = s.step3_peek().unwrap();
            assert_eq!(selected, a);
            let granted = request.try_read().unwrap().unwrap();
            assert!(granted.resources.is_empty());
            s.step4_resource_block(a, &granted, &response).unwrap();
            s.step5_guest_unblock(a, &granted, &response).unwrap();
            s.step6_reenquue(a, super::Requeue::Back);
            assert!(response.try_read().is_some());
            s.bump_global_time(&global, &Ok(granted));
            assert_eq!((s.committed_time - time).as_nanos(), 500_000);
        }
    }
    #[test]
    fn callback_checkpoint_mismatch_and_incomplete_do_not_invent_guest_events() {
        use crate::preemptions::callback_history::Checkpoint;
        use crate::preemptions::callback_history::Origin;
        use crate::preemptions::callback_history::Site;
        use crate::slice_boundary::callbacks::Callback;
        for hard in [false, true] {
            let t = tid(3);
            let mut scheduler = Scheduler::new(&Config::default());
            scheduler.replayer = Some(crate::scheduler::replayer::Replayer::new([
                SchedEvent::branches(t, 7),
            ]));
            assert!(!scheduler.replayer.as_ref().unwrap().die_on_desync);
            let point = Checkpoint {
                history_index: 0,
                origin: Origin {
                    callback: Callback::Cpuid,
                    site: Site::Prehook,
                    progress: 7,
                    last_op: Some(crate::types::Op::Branch),
                    time: LogicalTime::from_nanos(70),
                    rcbs: 7,
                },
            };
            scheduler.callback_checkpoints.insert(
                t,
                std::collections::VecDeque::from([
                    point.clone(),
                    Checkpoint {
                        history_index: 1,
                        ..point.clone()
                    },
                ]),
            );
            // The record would put checkpoint 0 at its schedule's event #0.
            scheduler.callback_checkpoint_events.insert((t, 0), 0);
            let before = format!("{:?}", scheduler.replayer);
            let turn = scheduler.turn;
            let time = scheduler.committed_time;
            let actual = Origin {
                time: LogicalTime::from_nanos(71),
                ..point.origin.clone()
            };
            scheduler.callback_checkpoint_mismatch(t, &point, hard, &actual);
            assert_eq!(format!("{:?}", scheduler.replayer), before);
            assert_eq!((scheduler.turn, scheduler.committed_time), (turn, time));
            let (stopped, _) = scheduler
                .generate_stopped_run_summary_for_log(None)
                .unwrap();
            assert_eq!(stopped.schedevent_desynced, 0);
            assert_eq!(stopped.schedevent_replayed, 0);
            let stopped_text = stopped.desync_descrip.unwrap();
            assert!(stopped_text.contains("DESYNC callback checkpoint"));
            assert!(stopped_text.contains(if hard { "(hard)" } else { "(soft)" }));
            assert!(!stopped_text.contains("DESYNC incomplete callback history"));
            let summary = scheduler.generate_partial_run_summary(None).unwrap();
            assert_eq!(summary.schedevent_desynced, 0);
            assert_eq!(summary.schedevent_replayed, 0);
            let text = summary.desync_descrip.unwrap();
            assert!(text.contains("DESYNC callback checkpoint"));
            assert!(text.contains(if hard { "(hard)" } else { "(soft)" }));
            assert!(text.contains("DESYNC incomplete callback history"));
            // Each line names where the record puts it and where the replay
            // was (https://github.com/rrnewton/hermit/issues/4876): the
            // mismatched checkpoint at the event ending its slice, the
            // unconsumed one with no recorded position, as this test's
            // custody has no record behind it.
            assert!(
                text.contains(
                    "DESYNC callback checkpoint 0 for 3 at record event #0 (reported at event \
                     #0, record event #0): expected"
                ),
                "{text}"
            );
            assert!(
                text.contains(
                    "DESYNC incomplete callback history for 3: 1 unconsumed checkpoints (1 (no \
                     recorded position)) (the replay ended at event #0, record event #0)"
                ),
                "{text}"
            );
            use detcore_model::summary::CheckpointRef;
            use detcore_model::summary::DesyncKind;
            use detcore_model::summary::ReplayPosition;
            let at_start = Some(ReplayPosition {
                event: 0,
                record_event: 0,
            });
            let notes: Vec<_> = summary
                .desync_notes
                .iter()
                .map(|note| {
                    assert!(
                        text.contains(note.message.as_str()),
                        "{note:?} not in {text}"
                    );
                    (
                        note.kind,
                        note.thread,
                        note.record_event,
                        note.checkpoints.clone(),
                        note.reported_at,
                    )
                })
                .collect();
            assert_eq!(
                notes,
                [
                    (
                        DesyncKind::IncompleteCallbackHistory,
                        Some(t),
                        None,
                        vec![CheckpointRef {
                            number: 1,
                            record_event: None
                        }],
                        at_start,
                    ),
                    (
                        DesyncKind::CallbackCheckpoint,
                        Some(t),
                        Some(0),
                        vec![CheckpointRef {
                            number: 0,
                            record_event: Some(0)
                        }],
                        at_start,
                    ),
                ]
            );
        }
    }

    /// A schedule replay's summary names where in the record each departure
    /// lies, in `desync_descrip` and `desync_notes`
    /// (https://github.com/rrnewton/hermit/issues/4876): the first replayed
    /// event that differed, with its thread and the record's; a mismatched
    /// checkpoint at the event the record ends its slice with, apart from the
    /// later point where the thread reported it. An event the record lacks
    /// sets the replay's numbering apart from the record's, and a
    /// fast-forward moves the record's position on.
    #[test]
    fn summarized_desyncs_name_their_recorded_event() {
        use detcore_model::summary::DesyncKind;

        use crate::preemptions::callback_history::Checkpoint;
        use crate::preemptions::callback_history::Origin;
        use crate::preemptions::callback_history::Site;
        use crate::slice_boundary::callbacks::Callback;
        let t = tid(3);
        let point = Checkpoint {
            history_index: 4,
            origin: Origin {
                callback: Callback::Cpuid,
                site: Site::Prehook,
                progress: 7,
                last_op: Some(crate::types::Op::Branch),
                time: LogicalTime::from_nanos(70),
                rcbs: 7,
            },
        };
        let actual = Origin {
            time: LogicalTime::from_nanos(71),
            ..point.origin.clone()
        };

        // The replayer needs each observed event's time.
        let event = |thread: DetTid, count: u32, nanos: u64| SchedEvent {
            end_time: Some(LogicalTime::from_nanos(nanos)),
            ..SchedEvent::branches(thread, count)
        };
        let mut scheduler = Scheduler::new(&Config::default());
        scheduler.replayer = Some(crate::scheduler::replayer::Replayer::new([
            event(t, 7, 70),
            event(t, 5, 120),
            event(t, 3, 150),
            event(t, 2, 170),
        ]));
        let replayer = scheduler.replayer.as_mut().unwrap();
        replayer.observe_event(&event(t, 7, 70));
        // Event #1 is not in the record: another thread ran. It consumes no
        // recorded event, so from here the replay's numbering runs one ahead
        // of the record's.
        replayer.observe_event(&event(tid(4), 2, 90));
        replayer.observe_event(&event(t, 5, 120));
        replayer.observe_event(&event(t, 3, 150));
        // A later desync (another count, a soft desync that still consumes
        // the recorded event) leaves the first in place.
        replayer.observe_event(&event(t, 9, 170));
        let first = replayer.first_desync.unwrap();
        assert_eq!(
            (
                first.event,
                first.record_event,
                first.observed,
                first.expected,
                first.hard
            ),
            (1, 1, tid(4), t, true)
        );
        assert_eq!(
            (replayer.traced_event_count, replayer.record_position()),
            (5, 4)
        );
        // A fast-forward past a recorded event the replay never ran moves the
        // record's position on by the events it drops.
        let mut skipping = crate::scheduler::replayer::Replayer::new([
            event(t, 7, 70),
            event(t, 5, 120),
            event(t, 3, 150),
        ]);
        skipping.observe_event(&event(t, 7, 70));
        skipping.observe_event(&event(t, 3, 150));
        assert_eq!(
            skipping
                .first_desync
                .map(|first| (first.event, first.record_event, first.hard)),
            Some((1, 1, false))
        );
        assert_eq!(
            (skipping.traced_event_count, skipping.record_position()),
            (2, 3)
        );
        // The record puts checkpoint 4 at its schedule's event #2; the thread
        // reports the mismatch later, at the replay's event #5 (record #4).
        scheduler
            .callback_checkpoints
            .insert(t, std::collections::VecDeque::from([point.clone()]));
        scheduler.callback_checkpoint_events.insert((t, 4), 2);
        scheduler.callback_checkpoint_mismatch(t, &point, false, &actual);
        let summary = scheduler.generate_partial_run_summary(None).unwrap();
        let text = summary.desync_descrip.unwrap();
        assert!(
            text.contains(
                "DESYNC first schedule event at record event #1 (event #1): thread 4 ran, the \
                 record has thread 3 (hard)\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "DESYNC callback checkpoint 4 for 3 at record event #2 (reported at event #5, \
                 record event #4): expected"
            ),
            "{text}"
        );
        let placed: Vec<_> = summary
            .desync_notes
            .iter()
            .map(|note| {
                (
                    note.kind,
                    note.thread,
                    note.record_event,
                    note.reported_at.map(|at| (at.event, at.record_event)),
                )
            })
            .collect();
        assert_eq!(
            placed,
            [
                (
                    DesyncKind::ScheduleEvent,
                    Some(tid(4)),
                    Some(1),
                    Some((1, 1))
                ),
                (
                    DesyncKind::CallbackCheckpoint,
                    Some(t),
                    Some(2),
                    Some((5, 4))
                ),
            ]
        );
    }

    /// One `desync_notes` collects every producer's note: a replay that
    /// departs from its schedule at an event and leaves a recorded background
    /// start untaken reports both, the schedule event's note first, in
    /// `desync_descrip`'s order (https://github.com/rrnewton/hermit/issues/4876).
    #[test]
    fn desync_notes_collect_every_producers_note() {
        use detcore_model::summary::DesyncKind;
        let (a, b, c) = (tid(3), tid(5), tid(7));
        let event = |thread: DetTid, count: u32, nanos: u64| SchedEvent {
            end_time: Some(LogicalTime::from_nanos(nanos)),
            ..SchedEvent::branches(thread, count)
        };
        let recorded = [event(a, 10, 100), event(a, 20, 200), event(a, 30, 300)];
        let start = |after, thread, call| crate::preemptions::BackgroundStart {
            after,
            thread,
            call,
        };
        let mut scheduler = Scheduler::new(&Config::default());
        scheduler.replayer = Some(
            crate::scheduler::replayer::Replayer::new(recorded.clone())
                .with_background_starts(&[start(1, b, 2)], true),
        );
        let replayer = scheduler.replayer.as_mut().unwrap();
        replayer.observe_event(&recorded[0]);
        // Another thread runs where the record has `a`: the start due after
        // event 0 is passed, and the event departs from the schedule.
        replayer.observe_event(&event(c, 20, 200));
        let summary = scheduler.generate_partial_run_summary(None).unwrap();
        let kinds: Vec<_> = summary.desync_notes.iter().map(|note| note.kind).collect();
        assert_eq!(
            kinds,
            [
                DesyncKind::ScheduleEvent,
                DesyncKind::UntakenBackgroundStarts
            ],
            "{:?}",
            summary.desync_descrip
        );
        let text = summary.desync_descrip.unwrap();
        for note in &summary.desync_notes {
            assert!(text.contains(&note.message), "{text}");
        }
    }
    /// A thread reported past a checkpoint that this custody no longer holds
    /// first is refused as overshot by name, with the policy exit, instead of
    /// the custody assertion's panic. Any other disagreement (here, a report
    /// at the checkpoint's own coordinates) is still that internal error. Each
    /// case runs in a re-executed copy of this test binary, because the
    /// refusal exits the process.
    #[test]
    fn a_mismatch_past_a_checkpoint_the_scheduler_no_longer_holds_is_refused_as_overshot() {
        use crate::preemptions::callback_history::Checkpoint;
        use crate::preemptions::callback_history::Origin;
        const ROLE: &str = "HERMIT_TEST_CALLBACK_MISMATCH_ROLE";
        const NAME: &str = "scheduler::callback_history_scheduler_tests::\
             a_mismatch_past_a_checkpoint_the_scheduler_no_longer_holds_is_refused_as_overshot";
        let t = tid(3);
        // The custody holds row 1 first; the report names row 0.
        let stale = Checkpoint {
            history_index: 0,
            origin: row_origin(20),
        };
        let child = |actual: Origin| {
            let mut s = annotated();
            s.install_priority_checkpoints(&checkpoints(&[(t, 1, 40)]))
                .unwrap();
            s.callback_checkpoint_mismatch(t, &stale, true, &actual);
        };
        match std::env::var(ROLE).as_deref() {
            Ok("past") => {
                child(Origin {
                    rcbs: 21,
                    ..row_origin(20)
                });
                unreachable!("an overshot report ends the run");
            }
            Ok("at") => {
                child(row_origin(20));
                unreachable!("a custody disagreement at the checkpoint panics");
            }
            _ => {}
        }
        let run = |role: &str| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(ROLE, role)
                .output()
                .unwrap()
        };
        let past = run("past");
        let stderr = String::from_utf8_lossy(&past.stderr);
        assert_eq!(
            past.status.code(),
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            "{stderr}"
        );
        assert!(
            stderr.contains("HERMIT_CALLBACK_CHECKPOINT_OVERSHOT: ")
                && stderr.contains("went past callback checkpoint 0"),
            "{stderr}"
        );
        assert!(!stderr.contains("panicked"), "{stderr}");
        let at = run("at");
        let stderr = String::from_utf8_lossy(&at.stderr);
        assert_ne!(
            at.status.code(),
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            "{stderr}"
        );
        assert!(
            stderr.contains("mismatch refers to the current callback checkpoint"),
            "{stderr}"
        );
    }
    #[test]
    fn callback_history_recording_activation_preserves_strict_and_history_modes() {
        use crate::preemptions::callback_history::recording_enabled;

        for timer in [false, true] {
            let mut cfg = Config {
                sequentialize_threads: true,
                record_preemptions: true,
                chaos: true,
                ..Config::default()
            }
            .with_backend(|backend| backend.delivers_preemption_timer_events = timer);
            assert!(recording_enabled(&cfg));
            assert!(crate::scheduler::Scheduler::new(&cfg).callback_history_active);
            // Priority-history replay is distinct from global schedule replay;
            // preserve the pre-existing recording producer's exact predicate.
            cfg.replay_preemptions_from = Some("history.json".into());
            assert!(recording_enabled(&cfg));
            cfg.chaos = false;
            assert!(!recording_enabled(&cfg));
            cfg.replay_preemptions_from = None;
            cfg.chaos = true;
            cfg.strict_schedule_boundaries = true;
            assert!(!recording_enabled(&cfg));
            // Strict+chaos is refused by the existing admission gate. The
            // supported strict recording mode is independently inactive too.
            assert!(crate::scheduler::validate_strict_boundary_config(&cfg).is_err());
            cfg.chaos = false;
            assert!(!crate::scheduler::Scheduler::new(&cfg).callback_history_active);

            // Load an admitted strict recording through the real scheduler;
            // the legacy progress mechanism remains inactive on replay too.
            use crate::preemptions::boundary_recording::BoundaryRecording;
            use crate::preemptions::boundary_recording::Recording;
            let typed = Recording::Boundaries(
                BoundaryRecording::new(
                    crate::preemptions::PreemptionRecord::default(),
                    detcore_model::schedule::boundary::Schedule::new(vec![]).unwrap(),
                )
                .unwrap(),
            );
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("strict.json");
            std::fs::write(&path, typed.encode().unwrap()).unwrap();
            cfg.record_preemptions = false;
            cfg.replay_schedule_from = Some(path);
            assert!(!crate::scheduler::Scheduler::new(&cfg).callback_history_active);
        }
    }

    fn annotated() -> Scheduler {
        Scheduler::new(&Config {
            annotated_priority_replay: true,
            ..Config::default()
        })
    }

    fn row_origin(rcbs: u64) -> crate::preemptions::callback_history::Origin {
        use crate::preemptions::callback_history::Origin;
        use crate::preemptions::callback_history::Site;
        use crate::slice_boundary::callbacks::Callback;
        Origin {
            callback: Callback::Syscall(Sysno::write),
            site: Site::Posthook,
            progress: rcbs + 2,
            last_op: Some(crate::types::Op::Syscall(
                Sysno::write,
                SyscallPhase::Posthook,
            )),
            time: LogicalTime::from_nanos(rcbs * 10),
            rcbs,
        }
    }

    fn checkpoints(
        points: &[(DetTid, usize, u64)],
    ) -> BTreeMap<DetTid, Vec<crate::preemptions::callback_history::Checkpoint>> {
        let mut map: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (tid, history_index, rcbs) in points {
            map.entry(*tid)
                .or_default()
                .push(crate::preemptions::callback_history::Checkpoint {
                    history_index: *history_index,
                    origin: row_origin(*rcbs),
                });
        }
        map
    }

    fn row_request(t: DetTid, row: u64, origin: Option<u64>) -> Resources {
        let mut rs = priority(t, false);
        rs.priority_row = Some(row);
        rs.callback_checkpoint = origin.map(|rcbs| Box::new(row_origin(rcbs)));
        rs
    }

    /// The scheduler acknowledges each recorded row once, at the selection of
    /// the priority request that ends it: an unannotated row counts and
    /// consumes nothing, an annotated row consumes its checkpoint only with
    /// its exact recorded origin, and an origin the recorder attached to an
    /// unannotated row consumes nothing either.
    #[test]
    fn annotated_priority_rows_are_acknowledged_once_with_their_checkpoints() {
        let t = tid(3);
        let mut s = annotated();
        s.install_priority_checkpoints(&checkpoints(&[(t, 1, 40)]))
            .unwrap();
        s.acknowledge_priority_row(t, &row_request(t, 0, None))
            .unwrap();
        assert_eq!(s.priority_rows[&t], 1);
        assert_eq!(s.callback_checkpoints[&t].len(), 1);
        s.acknowledge_priority_row(t, &row_request(t, 1, Some(40)))
            .unwrap();
        assert_eq!(s.priority_rows[&t], 2);
        assert!(s.callback_checkpoints[&t].is_empty());
        s.acknowledge_priority_row(t, &row_request(t, 2, Some(70)))
            .unwrap();
        assert_eq!(s.priority_rows[&t], 3);
        // A request without a row (a final slice, an ordinary yield) counts
        // nothing.
        s.acknowledge_priority_row(t, &priority(t, false)).unwrap();
        assert_eq!(s.priority_rows[&t], 3);
        assert_eq!(s.priority_history_incomplete(), None);

        // Off the annotated mode nothing is acknowledged.
        let mut legacy = Scheduler::new(&Config::default());
        legacy
            .acknowledge_priority_row(t, &row_request(t, 5, None))
            .unwrap();
        assert!(legacy.priority_rows.is_empty());
    }

    /// Every custody fault is a named refusal that advances nothing: a row
    /// other than the next one, an annotated row ended without its endpoint
    /// or at another origin, and an earlier row's checkpoint left unconsumed.
    #[test]
    fn annotated_priority_acknowledgement_refuses_custody_faults() {
        let t = tid(3);
        for (request, rows_before, name) in [
            (row_request(t, 1, None), 0, "SKIPPED"),
            (row_request(t, 1, None), 1, "SKIPPED"),
            (row_request(t, 1, Some(41)), 1, "INCOMPATIBLE"),
            (row_request(t, 2, None), 2, "SKIPPED"),
        ] {
            let mut s = annotated();
            s.install_priority_checkpoints(&checkpoints(&[(t, 1, 40)]))
                .unwrap();
            if rows_before > 0 {
                s.priority_rows.insert(t, rows_before);
            }
            let error = s.acknowledge_priority_row(t, &request).unwrap_err();
            assert!(
                error.starts_with(&format!("HERMIT_CALLBACK_CHECKPOINT_{name}")),
                "{error}"
            );
            assert_eq!(s.priority_rows.get(&t).copied().unwrap_or(0), rows_before);
            assert_eq!(s.callback_checkpoints[&t].len(), 1);
        }
    }

    /// Detcore refuses an annotated run whose effective configuration cannot
    /// honor its row ends, whoever prepared it, before the scheduler exists:
    /// a configuration with no preemption timer events, and one without
    /// sequentialized threads, are named. The same configuration with
    /// ptrace's capabilities passes, and an unannotated one is not judged.
    #[test]
    fn detcore_refuses_an_annotated_run_on_an_unqualified_configuration() {
        let unprepared = Config {
            annotated_priority_replay: true,
            sequentialize_threads: true,
            ..Config::default()
        };
        let refusal = crate::preemptions::annotated_replay_mode_refusal(&unprepared).unwrap();
        assert!(
            refusal.starts_with("HERMIT_CALLBACK_CHECKPOINT_UNQUALIFIED_MODE")
                && refusal.contains("cannot deliver Detcore preemption timer events"),
            "{refusal}"
        );
        let qualified = Config {
            backend: reverie::BackendCapabilities::PTRACE,
            ..unprepared.clone()
        };
        assert_eq!(
            crate::preemptions::annotated_replay_mode_refusal(&qualified),
            None
        );
        let unsequentialized = Config {
            sequentialize_threads: false,
            ..qualified.clone()
        };
        let refusal = crate::preemptions::annotated_replay_mode_refusal(&unsequentialized).unwrap();
        assert!(
            refusal.contains("does not sequentialize threads (--no-sequentialize-threads)"),
            "{refusal}"
        );
        let unannotated = Config {
            annotated_priority_replay: false,
            ..unsequentialized
        };
        assert_eq!(
            crate::preemptions::annotated_replay_mode_refusal(&unannotated),
            None
        );
    }

    /// The run's checkpoints and its launch decision are one identity: an
    /// annotated run needs an annotated record and a legacy run a legacy one.
    /// A run ends with a typed refusal when checkpoints remain unconsumed.
    #[test]
    fn annotated_priority_custody_matches_the_launch_decision_and_its_end() {
        let t = tid(3);
        let annotated_map = checkpoints(&[(t, 0, 10), (t, 2, 30)]);
        let error = annotated()
            .install_priority_checkpoints(&BTreeMap::new())
            .unwrap_err();
        assert!(error.starts_with("HERMIT_CALLBACK_CHECKPOINT_UNQUALIFIED_MODE"));
        let error = Scheduler::new(&Config::default())
            .install_priority_checkpoints(&annotated_map)
            .unwrap_err();
        assert!(error.starts_with("HERMIT_CALLBACK_CHECKPOINT_UNQUALIFIED_MODE"));
        let mut legacy = Scheduler::new(&Config::default());
        legacy
            .install_priority_checkpoints(&BTreeMap::new())
            .unwrap();
        assert!(legacy.callback_checkpoints.is_empty());

        let mut s = annotated();
        s.install_priority_checkpoints(&annotated_map).unwrap();
        assert!(s.callback_history_active);
        let refusal = s.priority_history_incomplete().unwrap();
        assert!(
            refusal.starts_with("HERMIT_CALLBACK_CHECKPOINT_INCOMPLETE"),
            "{refusal}"
        );
        assert!(
            refusal.contains("dtid 3: 2 unconsumed from row 0"),
            "{refusal}"
        );
        let error = s.generate_partial_run_summary(None).unwrap_err();
        assert!(error.is::<crate::PriorityHistoryRefusal>(), "{error:#}");
    }

    /// A timeout can stop a qualified annotated replay before its next
    /// checkpoint. Its summary keeps the unconsumed suffix, while claiming
    /// that the same replay finished remains a typed refusal.
    #[test]
    fn stopped_annotated_priority_replay_keeps_its_incomplete_history_in_the_summary() {
        let config = Config {
            annotated_priority_replay: true,
            sequentialize_threads: true,
            backend: reverie::BackendCapabilities::PTRACE,
            ..Config::default()
        };
        assert_eq!(config.annotated_priority_replay_unqualified(), None);
        let t = tid(3);
        let mut s = Scheduler::new(&config);
        s.install_priority_checkpoints(&checkpoints(&[(t, 0, 10), (t, 2, 30)]))
            .unwrap();
        s.acknowledge_priority_row(t, &row_request(t, 0, Some(10)))
            .unwrap();
        let checkpoints_before = s.callback_checkpoints.clone();
        let rows_before = s.priority_rows.clone();
        let time_before = s.committed_time;
        let refusal = s.priority_history_incomplete().unwrap();
        assert!(refusal.starts_with("HERMIT_CALLBACK_CHECKPOINT_INCOMPLETE"));
        assert!(refusal.contains("dtid 3: 1 unconsumed from row 2"));

        let (summary, info_description) = s.generate_stopped_run_summary_for_log(None).unwrap();
        assert_eq!(
            summary.desync_descrip.as_deref(),
            Some(format!("{refusal}\n").as_str())
        );
        assert_eq!(summary.schedevent_desynced, 0);
        assert!(summary.to_string().contains(&refusal));
        assert!(
            summary
                .info(info_description.as_deref())
                .to_string()
                .contains(&refusal)
        );
        assert_eq!(
            serde_json::to_value(&summary).unwrap()["desync_descrip"],
            format!("{refusal}\n")
        );
        assert_eq!(s.callback_checkpoints, checkpoints_before);
        assert_eq!(s.priority_rows, rows_before);
        assert_eq!(s.committed_time, time_before);

        let error = s.generate_partial_run_summary(None).unwrap_err();
        assert!(error.is::<crate::PriorityHistoryRefusal>(), "{error:#}");
        assert_eq!(
            error.to_string(),
            format!("terminal replay refusal: {refusal}")
        );
        assert_eq!(s.callback_checkpoints, checkpoints_before);
    }

    /// An annotated history replays from its first row only at a thread's
    /// birth: a second start of the same thread ID, or a start after its rows
    /// or progress were consumed, is refused by name.
    #[test]
    fn annotated_priority_history_starts_only_at_birth() {
        let (a, b, c) = (tid(3), tid(4), tid(5));
        let mut s = annotated();
        s.authenticate_priority_birth(a).unwrap();
        let error = s.authenticate_priority_birth(a).unwrap_err();
        assert!(
            error.starts_with("HERMIT_CALLBACK_CHECKPOINT_UNQUALIFIED_RESUME"),
            "{error}"
        );
        s.priority_rows.insert(b, 1);
        assert!(s.authenticate_priority_birth(b).is_err());
        s.callback_progress
            .entry(c)
            .or_default()
            .observe_event(&SchedEvent::branches(c, 1));
        assert!(s.authenticate_priority_birth(c).is_err());

        let mut legacy = Scheduler::new(&Config::default());
        legacy.authenticate_priority_birth(a).unwrap();
        legacy.authenticate_priority_birth(a).unwrap();
    }

    /// Selected through the ordinary turn, an annotated row's priority
    /// request makes its one recorded NONCOMMIT change point, consumes its
    /// checkpoint and counts its row, and adds no other turn.
    #[test]
    fn an_annotated_priority_row_is_acknowledged_by_its_ordinary_selection() {
        let a = tid(3);
        let mut s = annotated();
        s.install_priority_checkpoints(&checkpoints(&[(a, 0, 0)]))
            .unwrap();
        // The request's change point is at time 0 and RCB 0, the endpoint of
        // `row_origin(0)`, as the selected checkpoint branch requires.
        let request = row_request(a, 0, Some(0));
        install(
            &mut s,
            a,
            crate::scheduler::runqueue::DEFAULT_PRIORITY,
            request,
        );
        let turn = s.turn;
        let (selected, posted, response) = s.step3_peek().unwrap();
        assert_eq!(selected, a);
        let posted = posted.try_read().unwrap().unwrap();
        assert!(s.step4_resource_block(a, &posted, &response).is_err());
        assert_eq!(
            s.turn,
            turn + 1,
            "one skipped turn, the recorded change point"
        );
        assert!(s.callback_checkpoints[&a].is_empty());
        assert_eq!(s.priority_rows[&a], 1);
        let empty = s.next_turns[&a].req.try_read().unwrap().unwrap();
        assert!(empty.resources.is_empty());
    }

    /// A schedule replay whose next recorded event is `a`'s close posthook.
    fn replay_waiting_for(a: DetTid) -> Scheduler {
        let mut s = Scheduler::new(&Config::default());
        let mut a_post = SchedEvent::syscall(a, Sysno::close, SyscallPhase::Posthook);
        a_post.end_time = Some(LogicalTime::ZERO);
        s.replayer = Some(crate::scheduler::replayer::Replayer::new([
            a_post.clone(),
            a_post,
        ]));
        s
    }

    /// `spinner` takes `UNMATCHED_EVENTS_BEFORE_RELEASE` traced events in a
    /// row, none of them the schedule's next, and the replay releases the
    /// schedule at the last (`Replayer::release_schedule`).
    fn release_by_spinning(s: &mut Scheduler, spinner: DetTid) {
        let mut spin = SchedEvent::syscall(spinner, Sysno::sched_yield, SyscallPhase::Posthook);
        spin.end_time = Some(LogicalTime::ZERO);
        for _ in 1..crate::scheduler::replayer::UNMATCHED_EVENTS_BEFORE_RELEASE {
            s.consume_schedevent(&spin);
        }
        assert!(!s.schedule_released());
        assert!(s.consume_schedevent(&spin).keep_running);
        assert!(s.schedule_released());
    }

    /// A thread the replay holds back files a priority request that carries
    /// a callback checkpoint, so it has popped its own copy of the checkpoint
    /// already; the spinner's traced events then release the schedule, and
    /// only after that is the request selected. The scheduler's copy of the
    /// checkpoint must still be there for the request to consume: the thread
    /// has not heard of the release yet. Before, the release emptied every
    /// thread's scheduler copy at once and the selection panicked ("callback
    /// request has a recorded origin";
    /// https://github.com/rrnewton/hermit/pull/4615 review).
    #[test]
    fn a_checkpoint_request_filed_before_a_release_is_selected_after_it() {
        let a = tid(3);
        let b = tid(4);
        let mut s = replay_waiting_for(a);
        let mut a_request = priority(a, false);
        attach(&mut s, &mut a_request);
        install(&mut s, a, REPLAY_DEFERRED_PRIORITY, a_request);
        install(&mut s, b, REPLAY_FOREGROUND_PRIORITY, Resources::new(b));
        release_by_spinning(&mut s, b);
        s.logically_kill_thread(&b, &DetPid::from_raw(b.as_raw()), MmId::initial(b));
        s.drain_pending_run_queue_removals();
        let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        s.bump_global_time(&global, &Err(SkipTurn));
        let (selected, request, response) = s.step3_peek().unwrap();
        assert_eq!(selected, a);
        let posted = request.try_read().unwrap().unwrap();
        assert!(posted.callback_checkpoint.is_some());
        let turn = s.turn;
        assert!(
            s.step4_resource_block(a, &posted, &response).is_err(),
            "the change point is a skipped turn"
        );
        assert_eq!(s.turn, turn + 1);
        assert!(s.callback_checkpoints[&a].is_empty());
        assert_eq!(
            s.get_priority(a),
            crate::scheduler::runqueue::DEFAULT_PRIORITY,
            "past the release the change point takes the default priority"
        );
    }

    /// The other half of the same window: a thread that has not heard of the
    /// release yet checks its own copy of a checkpoint at a callback and
    /// reports a mismatch. The scheduler pops its copy of the same checkpoint
    /// and records the DESYNC line. Before, the release had emptied the
    /// scheduler's copy and the report hit the custody assertion.
    #[test]
    fn a_checkpoint_mismatch_reported_after_a_release_finds_its_checkpoint() {
        use crate::preemptions::callback_history::Checkpoint;
        let a = tid(3);
        let b = tid(4);
        let mut s = replay_waiting_for(a);
        let point = Checkpoint {
            history_index: 0,
            origin: row_origin(20),
        };
        s.callback_checkpoints
            .insert(a, std::collections::VecDeque::from([point.clone()]));
        install(&mut s, a, REPLAY_DEFERRED_PRIORITY, Resources::new(a));
        install(&mut s, b, REPLAY_FOREGROUND_PRIORITY, Resources::new(b));
        release_by_spinning(&mut s, b);
        let actual = crate::preemptions::callback_history::Origin {
            time: LogicalTime::from_nanos(201),
            ..point.origin.clone()
        };
        s.callback_checkpoint_mismatch(a, &point, true, &actual);
        assert!(s.callback_checkpoints[&a].is_empty());
        let text = s
            .generate_partial_run_summary(None)
            .unwrap()
            .desync_descrip
            .unwrap();
        assert!(
            text.contains("DESYNC callback checkpoint 0 for 3"),
            "{text}"
        );
    }

    /// The end of a timeslice armed with `--chaos-instruction-offset`
    /// publishes where its timer was armed just before its change point
    /// (`Scheduler::publish_instruction_offset`), and the change point records
    /// it on its row and takes it; the same change point without one records
    /// none.
    #[test]
    fn a_change_point_records_the_instruction_offset_it_carries() {
        use crate::resources::InstructionOffset;
        let thread = tid(3);
        let offset = InstructionOffset {
            rcb_target: 40,
            instructions: 3,
        };
        for published in [Some(offset), None] {
            let mut s = Scheduler::new(&Config {
                record_preemptions: true,
                ..Config::default()
            });
            s.preemption_writer
                .as_mut()
                .unwrap()
                .register_thread(thread, 200);
            // The change point ends the slice at branch 43, three steps
            // past the target.
            let mut request = Resources::new(thread);
            request.insert(
                ResourceID::PriorityChangePoint(200, LogicalTime::ZERO, 43, Vec::new()),
                Permission::W,
            );
            install(&mut s, thread, 200, request);
            if let Some(offset) = published {
                assert!(s.publish_instruction_offset(thread, MmId::initial(thread), offset));
            }
            let (selected, filed, response) = s.step3_peek().unwrap();
            assert_eq!(selected, thread);
            let filed = filed.try_read().unwrap().unwrap();
            // A change point ends the turn whether or not it carries an offset.
            assert!(s.step4_resource_block(thread, &filed, &response).is_err());
            let record: serde_json::Value =
                serde_json::from_str(&s.preemption_writer.take().unwrap().into_string()).unwrap();
            let offsets =
                record["per_thread"][thread.to_string()]["preemption_instruction_offsets"].clone();
            assert!(s.instruction_offsets.is_empty());
            let expected = match published {
                Some(_) => serde_json::json!([{"rcb_target": 40, "instructions": 3}]),
                None => serde_json::Value::Null,
            };
            assert_eq!(offsets, expected, "{record}");
        }
    }

    /// A published offset dies with its thread
    /// (`Scheduler::logically_kill_thread`), and nothing publishes one for a
    /// thread the scheduler does not know, one logically killed, or a retired
    /// exec incarnation of its tid: a later thread under that tid must not
    /// record it on its own row.
    #[test]
    fn a_published_instruction_offset_dies_with_its_thread() {
        use crate::resources::InstructionOffset;
        let thread = tid(3);
        let mm = MmId::initial(thread);
        let offset = InstructionOffset {
            rcb_target: 40,
            instructions: 3,
        };
        let mut s = Scheduler::new(&Config {
            record_preemptions: true,
            ..Config::default()
        });
        assert!(!s.publish_instruction_offset(thread, mm, offset), "unknown");
        install(&mut s, thread, 200, priority(thread, false));
        assert!(s.publish_instruction_offset(thread, mm, offset));
        assert_eq!(s.instruction_offsets.get(&thread), Some(&offset));
        s.logically_kill_thread(&thread, &DetPid::from_raw(3), mm);
        assert!(s.instruction_offsets.is_empty());
        assert!(!s.publish_instruction_offset(thread, mm, offset), "killed");

        let mut s = Scheduler::new(&Config::default());
        install(&mut s, thread, 200, priority(thread, false));
        let successor = MmId::initial(tid(4));
        s.exec_incarnations.insert(thread, successor);
        assert!(!s.publish_instruction_offset(thread, mm, offset), "retired");
        assert!(s.instruction_offsets.is_empty());
        assert!(s.publish_instruction_offset(thread, successor, offset));
    }
}
