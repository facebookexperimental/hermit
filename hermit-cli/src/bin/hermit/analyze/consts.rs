/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Constants used by analyze.

// We identify a run by a root file name, and then append a standard set of suffixes to store the
// associated files for that run.
pub const LOG_EXT: &str = "log";
pub const PREEMPTS_EXT: &str = "preempts";
pub const PREEMPTS_EXT_PRESTRIPPED: &str = "preempts_with_times";
pub const SCHED_EXT: &str = "events";
