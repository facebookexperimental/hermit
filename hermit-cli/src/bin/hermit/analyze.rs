/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A mode for analyzing a hermit run to detect concurrency bugs.

mod consts;
mod memory;
mod minimize;
mod phases;
// The report (the next change in this stack) is the first caller.
#[cfg_attr(not(test), allow(dead_code))]
mod races;
mod rundata;
mod stop;
mod types;
mod workspace;

pub use memory::MemoryBudget;
pub use types::AnalyzeOpts;
pub use types::ContinuationTimeslice;
pub use types::ExitStatusConstraint;
pub use types::TrialTimeout;
