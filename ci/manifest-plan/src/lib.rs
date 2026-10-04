/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub mod backend_parity;
mod backend_parity_policy;
#[path = "../../../hermit-cli/src/canonical_verdict.rs"]
pub mod canonical_verdict;
pub mod ci_selection;
pub mod cli_help;
pub mod cpu_evidence;
pub mod environmental_block;
mod git_environment;
pub mod host_capability;
pub mod ledger;
#[path = "../../../hermit-cli/src/logdiff_report.rs"]
pub mod logdiff_report;
pub mod manifest_corpus;
pub mod manifest_metadata;
pub mod manifest_value;
pub mod nextest_binaries;
mod nextest_build_selections;
pub mod nextest_cohort;
pub mod nextest_cpu;
pub mod parity;
#[path = "../../../hermit-cli/tests/common/proc_locks_lease.rs"]
mod proc_locks_lease;
pub mod retired_ids;
pub mod runner;
pub mod self_test_selection;
pub mod service_result;
pub mod stress_series;
pub mod timeouts;
pub mod validation_dag;
mod validation_dag_static;
pub mod validation_inventory;
