/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Constants used by analyze.

use crate::global_opts::GlobalOpts;

/// Right now we don't want turning on logging for `hermit analyze` itself to ALSO turn on logging
/// for each one of the (many) individual hermit executions it calls.  This could change in the
/// future and instead share the GlobalOpts passed to `main()`.
pub const NO_LOGGING: GlobalOpts = GlobalOpts {
    log: None,
    log_file: None,
};

// We identify a run by a root file name, and then append a standard set of suffixes to store the
// associated files for that run.
pub const LOG_EXT: &str = "log";
pub const PREEMPTS_EXT: &str = "preempts";
pub const SCHED_EXT: &str = "events";
