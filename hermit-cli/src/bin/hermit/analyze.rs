/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A mode for analyzing a hermit run to detect concurrency bugs.

mod budget;
mod checkpoint;
mod consts;
mod memory;
mod minimize;
mod phases;
mod race_report;
mod races;
mod rundata;
mod stop;
mod types;
mod verdict;
mod workspace;

pub use budget::TimeBudget;
pub use budget::TrialBudget;
pub use memory::MemoryBudget;
pub use types::AnalyzeOpts;
pub use types::ContinuationTimeslice;
pub use types::ExitStatusConstraint;
pub use types::TrialTimeout;
pub use verdict::AnalyzeFailure;
pub use verdict::AnalyzePrecondition;
pub use verdict::AnalyzeVerdict;
pub use verdict::precondition_class;
