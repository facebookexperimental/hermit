// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! The size of the pinned reverie-dbt source inventory.
//!
//! scripts/patch-reverie-dbt-buck.rs checks it when it patches the generated
//! Buck graph, and scripts/build-buck-release.rs checks it again against the
//! manifest map in that graph before a Buck release build. Both read this one
//! constant, so a Reverie pin that adds or removes a reverie-dbt file changes
//! the count in one place: `patch-reverie-dbt-buck.rs --check-inventory`, which
//! `make lint-checks` runs, fails until it is updated.

/// `git ls-files -- reverie-dbt` in the pinned Reverie checkout lists this
/// many files. Reverie 78e3cadac76b added
/// reverie-dbt/src/caller_runtime/tests/facility_admission.rs: 1044 + 1.
/// The DBT caller stack (Reverie 10e2e5c389) added
/// reverie-dbt/tests/caller_thread_start_tail.rs and
/// reverie-dbt/tests/fixtures/thread-start-tail-entry.S: 1045 + 2.
pub const REVERIE_DBT_FILES: usize = 1047;
