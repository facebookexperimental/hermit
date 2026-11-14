/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Test-only public associated-type lifecycle shared by library and bin tests.
//! Each finding/refusal/error is produced by the actual coordinator receiver
//! and joined cleanup. Stage1 separately covers the private stale producer.
//! This module executes no guest process or backend.

use std::time::Duration;

use anyhow::Error;
use detcore::Config;
use detcore::GlobalState;
use detcore::ReplayDepartureError;
use detcore::ThreadState;
use detcore::preemptions::PreemptionRecord;
use detcore::preemptions::ThreadHistory;
use detcore_model::pid::DetTid;
use detcore_model::syscall_histogram::SparseSyscallHistogram;
use reverie::GlobalTool;
use reverie::Tid;
use serde_json::Value;
use serde_json::json;

type Request = <GlobalState as GlobalTool>::Request;
type Response = <GlobalState as GlobalTool>::Response;

#[derive(Clone, Copy)]
pub(crate) enum Publication {
    None,
    Accepted,
    Advancing,
}

/// Deserialize the real public associated type; do not mirror the private
/// GlobalRequest enum or construct coordinator findings/errors in the fixture.
fn request(thread: &ThreadState<()>, leaf: Value) -> Request {
    serde_json::from_value(json!([thread.thread_logical_time, thread.mm_id, leaf,])).unwrap()
}

/// These modeled lifecycle requests must actually acquire their normal grant.
/// This configuration has no deadline or boundary budget. Only the documented
/// optional lone-owner wrapper around Granted is accepted. Diagnostic replies
/// deliberately do not use this unwrapping helper.
fn granted_lifecycle_reply(thread: &mut ThreadState<()>, response: Response) -> Value {
    let (time, reply) = response;
    if let Some(time) = time {
        thread.thread_logical_time.advance_to(time);
    }
    let mut reply = serde_json::to_value(reply).unwrap();
    if let Some(inner) = reply.get("WithLoneGrant") {
        assert_eq!(reply.as_object().unwrap().len(), 1);
        reply = inner.clone();
    }
    assert_eq!(reply.as_object().unwrap().len(), 1);
    reply
        .get("Granted")
        .expect("registration/start must receive the actual scheduler grant")
        .clone()
}

/// Every result comes from a fresh production coordinator. No private state,
/// manual flush, cancellation or later Resources request installs the finding.
/// `bad_summary` supplies an existing directory as the real summary destination
/// to obtain an actual I/O failure after recording finalization.
pub(crate) async fn receiver_cleanup(
    publication: Publication,
    bad_summary: bool,
) -> (Result<(), Error>, Option<u64>) {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.json");
    let output = directory.path().join("output.json");
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        target_timeslice: std::num::NonZeroU64::new(100_000),
        panic_on_rcb_overshoot: false,
        replay_preemptions_from: Some(input.clone()),
        record_preemptions: true,
        record_preemptions_to: Some(output.clone()),
        ..Config::default()
    };
    // Prepared for this host's clock as `prepare_backend_config` prepares a
    // ptrace run, so the qualified history below is admitted.
    let prepared = crate::prepare_backend_config(Config::default(), crate::Backend::Ptrace);
    config.backend.rcb_clock_contract = prepared.backend.rcb_clock_contract;
    config.rcb_clock_claim = prepared.rcb_clock_claim;
    let tid = DetTid::from_raw(3);
    let from = Tid::from_raw(tid.as_raw());
    let mut thread = ThreadState::new(tid, &config, ());
    let epoch = thread.thread_logical_time.as_nanos();
    let priority = ThreadHistory::new().final_prio;
    let history: ThreadHistory = serde_json::from_value(json!({
        "final_prio": priority,
        "prio_changes": [
            [epoch + Duration::from_nanos(1_000_000), priority],
            [epoch + Duration::from_nanos(2_000_000), priority + 1],
        ],
        "preemption_rcbs": [1, 1],
        "chaos_epochs": [],
    }))
    .unwrap();
    let record: PreemptionRecord = serde_json::from_value::<PreemptionRecord>(json!({
        "per_thread": {"3": history},
        "global": [],
    }))
    .unwrap()
    .with_identity(Some(detcore::preemptions::HistoryIdentity::Known(
        reverie::pmu_config::RcbMetric::ConditionalBranchesRetiredUserV1,
    )));
    record.validate().unwrap();
    record.require_faithful_replay().unwrap();
    record.write_to_disk(&input).unwrap();

    let state = <GlobalState as GlobalTool>::init_global_state(&config).await;
    let registration = request(
        &thread,
        json!({"CreateChildThread": [tid, tid, 0, null, libc::SIGCHLD, null, null]}),
    );
    let reply = state.receive_rpc(from, registration).await;
    assert_eq!(
        granted_lifecycle_reply(&mut thread, reply),
        json!({"CreateChildThread": null}),
    );
    let start = request(&thread, json!({"StartNewThread": [tid, tid, null, null]}));
    let reply = state.receive_rpc(from, start).await;
    assert_eq!(
        granted_lifecycle_reply(&mut thread, reply),
        json!({"StartNewThread": [history, null]}),
        "startup must return this actual admitted history",
    );

    if !matches!(publication, Publication::None) {
        let previous = thread.thread_logical_time.as_nanos();
        let observed = match publication {
            Publication::Accepted => previous,
            Publication::Advancing => previous + Duration::from_nanos(1),
            Publication::None => unreachable!(),
        };
        let report = request(
            &thread,
            json!({"ReplayDeparture": {
                "previous": previous,
                "observed": observed,
                "slice": thread.stats.timeslice_count,
            }}),
        );
        let reply = serde_json::to_value(state.receive_rpc(from, report).await).unwrap();
        let acknowledgment = match publication {
            Publication::Accepted => json!({"Ok": null}),
            Publication::Advancing => json!({"Err": "Advancing"}),
            Publication::None => unreachable!(),
        };
        assert_eq!(reply, json!([null, {"ReplayDeparture": acknowledgment}]));
    }

    // This is the sole message after publication: actual matching deregistration
    // with the constructed thread's real accounting, never a manual state edit.
    let histogram = SparseSyscallHistogram::from(&thread.stats.syscall_histogram);
    let deregistration = request(
        &thread,
        json!({"DeregisterThread": {
            "dettid": tid,
            "detpid": tid,
            "mm": thread.mm_id,
            "thread_start_entered": true,
            "vfork_origin": null,
            "timeslice_stats": thread.stats.timeslice_stats,
            "slice_ran_past_bound": thread.stats.ran_past_bound,
            "syscall_count": thread.stats.syscall_count,
            "syscall_histogram": histogram,
            "chaos_epochs": [],
            "killed_by_group_signal": false,
            "passed_descriptors_out": false,
            "wait_status": 0,
        }}),
    );
    assert_eq!(
        serde_json::to_value(state.receive_rpc(from, deregistration).await).unwrap(),
        json!([null, {"DeregisterThread": null}]),
    );
    let destination = bad_summary.then(|| directory.path().to_path_buf());
    let cleanup = state.clean_up(false, &destination).await;
    let rerecorded: PreemptionRecord = detcore::preemptions::PreemptionReader::try_new(&output)
        .unwrap()
        .into_inner();
    assert!(rerecorded.schedevents().is_empty());
    (cleanup, rerecorded.replay_departed_at())
}

pub(crate) async fn ordinary_cleanup() -> Result<(), Error> {
    let (cleanup, marker) = receiver_cleanup(Publication::None, false).await;
    assert_eq!(marker, None);
    cleanup
}

pub(crate) async fn accepted_cleanup() -> Result<(), Error> {
    let (cleanup, marker) = receiver_cleanup(Publication::Accepted, false).await;
    assert_eq!(marker, Some(0));
    let error = cleanup.expect_err("the real receiver's accepted finding must refuse cleanup");
    let finding = ReplayDepartureError::in_error(&error)
        .unwrap()
        .summary()
        .expect("an accepted publication has a real summary");
    assert_eq!(finding.count, 1);
    assert_eq!(finding.first.thread, DetTid::from_raw(3));
    assert_eq!(
        finding.first.source,
        detcore_model::replay_departure::Source::ThreadFiling,
    );
    assert_eq!(finding.first.observed, finding.first.previous);
    assert_eq!(finding.first.recorded_event_count, marker);
    Err(error)
}
