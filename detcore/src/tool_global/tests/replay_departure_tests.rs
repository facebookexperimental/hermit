/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A modeled stale filing through the actual producer, RPC receiver and cleanup.
//! No backend or guest process is executed by this fixture.

use std::sync::Mutex;
use std::time::Duration;

use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Tid;
use reverie::Tool;

use super::ExternalRegistrationGuest;
use crate::Config;
use crate::Detcore;
use crate::GlobalState;
use crate::preemptions::PreemptionRecord;
use crate::preemptions::ThreadHistory;
use crate::preemptions::boundary_recording::Recording;
use crate::scheduler::DEFAULT_PRIORITY;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::tool_global::ThreadDeregistration;
use crate::types::DetTid;

fn is_resource_request(request: &GlobalRequest) -> bool {
    match request {
        GlobalRequest::WithForwardedCount(_, inner) => is_resource_request(inner),
        GlobalRequest::RequestResources(..) => true,
        _ => false,
    }
}

/// Drive the real stale-filing branch with a modeled Guest and the real
/// coordinator. This is not a naturally observed PMU skid: the previous
/// filing is staged through the production filing method at the current
/// clock (or one nanosecond earlier for the successful control).
///
/// Registration, the startup grant, request delivery and deregistration
/// all use production RPCs. Cleanup joins its own scheduler task normally;
/// no test hook transfers a finding or flushes a pending Resource request.
async fn stale_filing_cleanup(stale: bool) -> (anyhow::Result<()>, Option<u64>, u64) {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.json");
    let output = directory.path().join("output.json");
    let config = crate::clock_identity::test_sources::qualified(Config {
        sequentialize_threads: true,
        max_timeslice: None,
        target_timeslice: std::num::NonZeroU64::new(100_000),
        panic_on_rcb_overshoot: false,
        replay_preemptions_from: Some(input.clone()),
        record_preemptions: true,
        record_preemptions_to: Some(output.clone()),
        ..Config::default()
    });
    let tid = DetTid::from_raw(3);
    let mut thread = crate::ThreadState::new(tid, &config, ());
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
    // Go through the actual record decoder, validator, writer and startup
    // history loader. Equal RCB targets are permitted; times increase.
    let record: PreemptionRecord = serde_json::from_value::<PreemptionRecord>(serde_json::json!({
        "per_thread": {"3": history},
        "global": [],
    }))
    .unwrap()
    .with_identity(Some(crate::clock_identity::test_sources::known()));
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
        None,
    );
    let (history, first_slice) =
        crate::tool_global::thread_start_request(&config, &mut guest, tid).await;
    assert!(
        history.is_some(),
        "startup must load the actual input history"
    );
    crate::tool_global::finish_thread_start(&config, &mut guest, history, first_slice);
    assert_eq!(
        guest.thread.end_of_timeslice,
        Some(epoch + Duration::from_nanos(1_000_000)),
    );
    assert_eq!(guest.thread.replay_rcb_end, Some(1));
    assert_eq!(guest.thread.stats.timeslice_count, 1);
    {
        let scheduler = state.sched.lock().unwrap();
        assert!(scheduler.thread_was_registered(tid));
        assert_eq!(scheduler.thread_mm(tid), Some(guest.thread.mm_id));
        assert!(scheduler.rpc_incarnation_matches(tid, guest.thread.mm_id));
        assert!(scheduler.holds_serial_grant(tid));
        assert!(!scheduler.backend_failed());
    }
    let current = epoch + Duration::from_nanos(10_000);
    guest.thread.thread_logical_time.advance_to(current);
    assert_eq!(guest.thread.committed_clock_value, 0);
    assert_eq!(
        guest.thread.file_replay_change_point(
            &config,
            if stale {
                current
            } else {
                epoch + Duration::from_nanos(9_999)
            },
        ),
        Ok(()),
    );
    let before = guest.requests.lock().unwrap().len();
    let observed_event_count = state.sched.lock().unwrap().recorded_event_count;
    let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
    tool.end_timeslice_with_checkpoint(&mut guest, false, None, crate::CheckIn::Event, None)
        .await;
    let resource_requests = guest.requests.lock().unwrap()[before..]
        .iter()
        .filter(|request| is_resource_request(request))
        .count();
    assert_eq!(resource_requests, usize::from(!stale));
    assert_eq!(guest.thread.stats.last_recorded_slice.is_some(), stale);
    assert_eq!(guest.thread.stats.timeslice_count, 2);
    if stale {
        assert_eq!(
            guest.thread.end_of_timeslice,
            Some(crate::types::LogicalTime::MAX)
        );
        assert_eq!(guest.thread.replay_rcb_end, None);
    } else {
        assert_eq!(
            guest.thread.end_of_timeslice,
            Some(epoch + Duration::from_nanos(2_000_000)),
        );
        assert_eq!(guest.thread.replay_rcb_end, Some(1));
    }

    // The only later message is actual lifetime teardown. In particular,
    // it cannot make an unreported finding visible via another Resource.
    let response = guest
        .send_rpc((
            guest.thread.thread_logical_time.clone(),
            guest.thread.mm_id,
            GlobalRequest::DeregisterThread(ThreadDeregistration {
                dettid: tid,
                detpid: tid,
                mm: guest.thread.mm_id,
                thread_start_entered: true,
                passed_descriptors_out: false,
                wait_status: 0,
                vfork_origin: None,
                timeslice_stats: guest.thread.stats.timeslice_stats,
                syscall_count: guest.thread.stats.syscall_count,
                syscall_histogram: (&guest.thread.stats.syscall_histogram).into(),
                chaos_epochs: Vec::new(),
                killed_by_group_signal: false,
            }),
        ))
        .await;
    assert_eq!(response, (None, GlobalResponse::DeregisterThread(())));
    assert_eq!(
        guest.requests.lock().unwrap()[before..]
            .iter()
            .filter(|request| is_resource_request(request))
            .count(),
        resource_requests,
    );
    drop(guest);
    let cleanup = state.clean_up(false, &None).await;
    let rerecorded = Recording::read(&output).unwrap();
    (
        cleanup,
        rerecorded.replay_departed_at(),
        observed_event_count,
    )
}

#[tokio::test]
async fn a_stale_replay_filing_refuses_cleanup_without_another_resource_request() {
    let (ordinary, marker, _) = stale_filing_cleanup(false).await;
    ordinary.expect("a non-stale filing must complete real cleanup successfully");
    assert_eq!(marker, None);
    let (cleanup, _, _) = stale_filing_cleanup(true).await;
    let error = cleanup.expect_err("a stale filing must refuse at actual cleanup");
    assert!(
        error.to_string().starts_with("HERMIT_REPLAY_DEPARTURE:"),
        "the refusal must identify replay departure, not an unrelated cleanup failure: {error:#}",
    );
}

#[tokio::test]
async fn a_stale_replay_filing_marks_its_record_without_another_resource_request() {
    let (ordinary, marker, _) = stale_filing_cleanup(false).await;
    ordinary.expect("a non-stale filing must complete real cleanup successfully");
    assert_eq!(marker, None);
    let (cleanup, marker, observed_event_count) = stale_filing_cleanup(true).await;
    assert_eq!(
        marker,
        Some(observed_event_count),
        "the output must retain departure at its own observed event count",
    );
    let error = cleanup.expect_err("the marked run must refuse at actual cleanup");
    assert!(error.to_string().starts_with("HERMIT_REPLAY_DEPARTURE:"));
}
