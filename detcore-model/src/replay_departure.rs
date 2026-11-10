/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Bounded observations of a faithful preemption replay leaving its history.
//! These observations do not establish a timer cause or a common replay prefix.

use serde::Deserialize;
use serde::Serialize;

use crate::pid::DetTid;
use crate::time::LogicalTime;

/// The production point that observed a nonadvancing change point.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Source {
    /// The thread could not file its next change point.
    ThreadFiling,
    /// The scheduler omitted a change point from its output history.
    SchedulerChangePoint,
}

/// One observed change point, with coordinates from the observing run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Finding {
    /// The production observer, selected by the coordinator.
    pub source: Source,
    /// The authenticated thread, not an independently supplied payload ID.
    pub thread: DetTid,
    /// The preceding change point's logical time.
    pub previous: LogicalTime,
    /// The new change point's logical time, at or before `previous`.
    pub observed: LogicalTime,
    /// The thread's slice counter where the thread itself observed the finding.
    pub slice: Option<u64>,
    /// The new recording's observed event count at acceptance, if recording.
    /// This is not an input-history index or an exact common-prefix length.
    pub recorded_event_count: Option<u64>,
}

/// A bounded run diagnostic: detections are counted, only the first is retained.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Summary {
    /// Number of accepted detections, not distinct physical causes.
    pub count: u64,
    /// The first accepted observation in the run's deterministic order.
    pub first: Finding,
}
