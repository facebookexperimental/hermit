/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The `nightly` Cargo feature requests libtest output-capture clearing,
//! which needs std's unstable `internal_output_capture`. A feature alone
//! cannot express "nightly channel": `--all-features` on stable would turn
//! the feature on and fail with E0554. Detect the channel here and set
//! `detcore_testutils_nightly` only where rustc will allow this crate
//! unstable features: as `src/bootstrap.rs` decides from `RUSTC_BOOTSTRAP`
//! and the compiler's channel, exactly as rustc does. Stable builds compile
//! the capture code out exactly as they do with the feature off.

use std::process::Command;

include!("src/bootstrap.rs");

/// Whether `rustc` is a nightly or dev build, from its `-vV` output.
fn nightly_channel(rustc: &str) -> bool {
    let Ok(output) = Command::new(rustc).arg("-vV").output() else {
        return false;
    };
    let version = String::from_utf8_lossy(&output.stdout);
    version.contains("nightly") || version.contains("-dev")
}

fn main() {
    println!("cargo:rustc-check-cfg=cfg(detcore_testutils_nightly)");
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=RUSTC_BOOTSTRAP");

    let bootstrap = std::env::var("RUSTC_BOOTSTRAP").ok();
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    if unstable_features_allowed(
        bootstrap.as_deref(),
        "detcore_testutils",
        nightly_channel(&rustc),
    ) {
        println!("cargo:rustc-cfg=detcore_testutils_nightly");
    }
}
