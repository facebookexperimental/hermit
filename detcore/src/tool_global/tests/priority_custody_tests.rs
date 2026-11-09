/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The scheduler installs a callback-annotated priority history's
//! checkpoints from the same admitted object the per-thread replay is built
//! from, after its identity is admitted and the clock pinned.

use crate::Config;
use crate::GlobalState;
use crate::clock_identity::test_sources;

#[test]
fn checkpoints_install_from_the_admitted_priority_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("annotated.json");
    std::fs::write(
        &path,
        test_sources::annotated_history(Some(test_sources::known())),
    )
    .unwrap();
    let mut cfg = test_sources::qualified(Config {
        sequentialize_threads: true,
        replay_preemptions_from: Some(path),
        annotated_priority_replay: true,
        ..Config::default()
    });
    cfg.backend.delivers_preemption_timer_events = true;
    test_sources::with(Ok(test_sources::test_host()), |state| {
        let global = GlobalState::initialize(&cfg, false);
        assert_eq!(state.pinned(), Some(test_sources::test_host().key()));
        let sched = global.sched.lock().unwrap();
        let installed = sched
            .callback_checkpoints
            .iter()
            .map(|(tid, points)| (*tid, points.iter().cloned().collect::<Vec<_>>()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let admitted = global
            .preemptions_to_replay
            .as_ref()
            .expect("the admitted history is kept for replay")
            .callback_checkpoints();
        assert_eq!(&installed, admitted);
        assert_eq!(installed.values().map(Vec::len).sum::<usize>(), 1);
    });
}
