/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The identity a record-only run's history states is the one its actual
//! preparation committed, read back from the bytes its writer emits.

use reverie::BackendCapabilities;
use reverie::pmu_config::ClockKey;

use crate::Config;
use crate::GlobalState;
use crate::clock_identity::test_sources;
use crate::preemptions::HistoryIdentity;
use crate::preemptions::boundary_recording::Recording;

/// Initializes the global state for `cfg` against the default test host and a
/// fresh PMU state, and returns the identity its writer's output states and
/// the key left pinned.
fn emitted(cfg: Config) -> (Option<HistoryIdentity>, Option<ClockKey>) {
    test_sources::with(Ok(test_sources::test_host()), |state| {
        let global = GlobalState::initialize(&cfg, false);
        let written = global
            .sched
            .lock()
            .unwrap()
            .preemption_writer
            .take()
            .expect("a record-only run has a writer")
            .into_string();
        let identity = Recording::decode(written.as_bytes())
            .unwrap()
            .identity()
            .cloned();
        (identity, state.pinned())
    })
}

fn record_only() -> Config {
    Config {
        sequentialize_threads: true,
        record_preemptions: true,
        ..Config::default()
    }
}

/// A prepared run pins its key and states the committed metric, whether or
/// not Detcore observes the counter.
#[test]
fn a_prepared_record_only_run_writes_its_committed_identity() {
    for max_timeslice in [Config::default().max_timeslice, None] {
        let cfg = test_sources::qualified(Config {
            max_timeslice,
            ..record_only()
        });
        assert_eq!(
            emitted(cfg),
            (
                Some(test_sources::known()),
                Some(test_sources::test_host().key())
            )
        );
    }
}

/// An unprepared run, or one on a mechanism without the clock contract,
/// writes a history that states it is unqualified, and pins nothing.
#[test]
fn an_unqualified_record_only_run_writes_unqualified_and_pins_nothing() {
    let unqualified = Some(HistoryIdentity::Unqualified("claim-unset".to_owned()));
    assert_eq!(emitted(record_only()), (unqualified.clone(), None));
    let kvm = Config {
        backend: BackendCapabilities::KVM,
        ..record_only()
    };
    assert_eq!(emitted(kvm), (unqualified, None));
}
