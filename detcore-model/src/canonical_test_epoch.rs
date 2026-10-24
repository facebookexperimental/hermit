/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The one virtual-time epoch tests and E2E cells pass when they need a fixed
//! one, as `--epoch` or `HERMIT_EPOCH`: `hermit run --strict` refuses a run
//! whose epoch would come from the host clock. Bump it here and nowhere else.
//!
//! This file has no dependencies, so a rust-script that renders Hermit
//! commands includes it with `#[path]` rather than depending on this crate.

/// The canonical test epoch as a string literal, for `concat!`.
#[macro_export]
macro_rules! canonical_test_epoch {
    () => {
        "2026-01-01T00:00:00Z"
    };
}

/// [`canonical_test_epoch!`] as a value.
pub const CANONICAL_TEST_EPOCH: &str = canonical_test_epoch!();

/// [`canonical_test_epoch!`] as the `hermit run` option that passes it.
pub const CANONICAL_TEST_EPOCH_ARG: &str = concat!("--epoch=", canonical_test_epoch!());
