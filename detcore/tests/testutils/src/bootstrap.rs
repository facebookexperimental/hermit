/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Whether rustc allows unstable features to a crate, decided from
// `RUSTC_BOOTSTRAP` and the compiler's channel exactly as rustc decides it.
// `build.rs` includes this file; `tests/bootstrap_rule.rs` checks it.
//
// rustc's rule is `UnstableFeatures::from_environment_value` in
// `compiler/rustc_feature/src/lib.rs` (lines 62-85 at be8e82435e, the
// commit of the pinned formatter toolchain nightly-2026-07-12):
// - `1`, or a comma-separated list with a token equal to the crate's name,
//   allows them. Tokens are compared as they are, never trimmed, so
//   `"other, detcore_testutils"` does not name this crate.
// - `-1` forbids them, on a nightly compiler too.
// - Any other value, `0` and the empty string included, and an unset or
//   non-UTF-8 variable, leave the channel's default: allowed on nightly and
//   dev compilers, forbidden on stable and beta.

/// What a `RUSTC_BOOTSTRAP` value decides for one crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bootstrap {
    /// `1`, or a list naming the crate: unstable features are allowed.
    Allow,
    /// `-1`: unstable features are forbidden whatever the channel.
    Forbid,
    /// Anything else: the channel decides.
    ChannelDefault,
}

/// What `value`, the `RUSTC_BOOTSTRAP` variable as `std::env::var` read it,
/// decides for the crate named `krate`.
pub fn bootstrap_decision(value: Option<&str>, krate: &str) -> Bootstrap {
    match value {
        Some(value) if value == "1" || value.split(',').any(|name| name == krate) => {
            Bootstrap::Allow
        }
        Some("-1") => Bootstrap::Forbid,
        _ => Bootstrap::ChannelDefault,
    }
}

/// Whether rustc allows unstable features to `krate`, given `RUSTC_BOOTSTRAP`
/// and whether the compiler is a nightly or dev build.
pub fn unstable_features_allowed(value: Option<&str>, krate: &str, nightly_channel: bool) -> bool {
    match bootstrap_decision(value, krate) {
        Bootstrap::Allow => true,
        Bootstrap::Forbid => false,
        Bootstrap::ChannelDefault => nightly_channel,
    }
}
