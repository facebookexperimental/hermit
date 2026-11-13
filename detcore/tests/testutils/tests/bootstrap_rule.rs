/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The `RUSTC_BOOTSTRAP` rule that `build.rs` applies, from
//! `src/bootstrap.rs`. A Cargo-only test: the library does not compile that
//! file, so Buck's mirrored rule for the library needs no entry for it.

#[path = "../src/bootstrap.rs"]
mod bootstrap;

use bootstrap::*;

const KRATE: &str = "detcore_testutils";

/// Every value on both channels, against what rustc decides.
#[test]
fn each_bootstrap_value_decides_as_rustc_does_on_each_channel() {
    // (RUSTC_BOOTSTRAP, allowed on stable, allowed on nightly)
    let matrix: &[(Option<&str>, bool, bool)] = &[
        (None, false, true),
        (Some("1"), true, true),
        (Some("-1"), false, false),
        (Some("0"), false, true),
        (Some(""), false, true),
        (Some("detcore_testutils"), true, true),
        (Some("other,detcore_testutils"), true, true),
        (Some("detcore_testutils,other"), true, true),
        (Some("other, detcore_testutils"), false, true),
        (Some(" detcore_testutils"), false, true),
        (Some("detcore_testutils "), false, true),
        (Some("other"), false, true),
        (Some("detcore-testutils"), false, true),
        (Some("1,other"), false, true),
        (Some("-1,detcore_testutils"), true, true),
    ];
    for &(value, stable, nightly) in matrix {
        assert_eq!(
            unstable_features_allowed(value, KRATE, false),
            stable,
            "{value:?} on stable"
        );
        assert_eq!(
            unstable_features_allowed(value, KRATE, true),
            nightly,
            "{value:?} on nightly"
        );
    }
}

/// `-1` wins over a nightly compiler, as rustc's "hypnotize" arm does.
#[test]
fn minus_one_forbids_unstable_features_on_nightly() {
    assert_eq!(bootstrap_decision(Some("-1"), KRATE), Bootstrap::Forbid);
    assert!(!unstable_features_allowed(Some("-1"), KRATE, true));
}

/// A list token with whitespace around the crate's name does not name it.
#[test]
fn list_tokens_are_compared_untrimmed() {
    assert_eq!(
        bootstrap_decision(Some("unrelated_crate, detcore_testutils"), KRATE),
        Bootstrap::ChannelDefault
    );
    assert!(!unstable_features_allowed(
        Some("unrelated_crate, detcore_testutils"),
        KRATE,
        false
    ));
}
