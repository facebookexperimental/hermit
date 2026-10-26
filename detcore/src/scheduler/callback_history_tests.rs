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
            let summary = scheduler.generate_partial_run_summary(None).unwrap();
            assert_eq!(summary.schedevent_desynced, 0);
            assert_eq!(summary.schedevent_replayed, 0);
            let text = summary.desync_descrip.unwrap();
            assert!(text.contains("DESYNC callback checkpoint"));
            assert!(text.contains(if hard { "(hard)" } else { "(soft)" }));
            assert!(text.contains("DESYNC incomplete callback history"));
        }
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
            use crate::preemptions::boundary_recording::{BoundaryRecording, Recording};
            let typed = Recording::Boundaries(
                BoundaryRecording::new(
                    crate::preemptions::PreemptionRecord::default(),
                    detcore_model::schedule::boundary::Schedule::new(vec![]).unwrap(),
                ).unwrap(),
            );
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("strict.json");
            std::fs::write(&path, typed.encode().unwrap()).unwrap();
            cfg.record_preemptions = false;
            cfg.replay_schedule_from = Some(path);
            assert!(!crate::scheduler::Scheduler::new(&cfg).callback_history_active);
        }
    }
}
