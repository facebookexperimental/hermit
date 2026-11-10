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
//! `detcore_testutils_nightly` only on a nightly/dev compiler, or where
//! `RUSTC_BOOTSTRAP` actually permits unstable features for this crate,
//! so stable builds compile the capture code out exactly as they do with
//! the feature off. Bootstrap's value carries meaning: `1` permits every
//! crate, a comma-separated crate list permits only the named crates,
//! and `0`, `-1` or the empty string permit nothing.

use std::process::Command;

/// Whether a `RUSTC_BOOTSTRAP` value permits unstable features for this
/// crate (`detcore_testutils`): only `1` or a crate list naming it.
fn bootstrap_permits(value: Option<&str>) -> bool {
    match value {
        Some("1") => true,
        Some(list) => list
            .split(',')
            .any(|name| name.trim() == "detcore_testutils"),
        None => false,
    }
}

fn main() {
    println!("cargo:rustc-check-cfg=cfg(detcore_testutils_nightly)");
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=RUSTC_BOOTSTRAP");

    if bootstrap_permits(std::env::var("RUSTC_BOOTSTRAP").ok().as_deref()) {
        println!("cargo:rustc-cfg=detcore_testutils_nightly");
        return;
    }
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let Ok(output) = Command::new(rustc).arg("-vV").output() else {
        return;
    };
    let version = String::from_utf8_lossy(&output.stdout);
    if version.contains("nightly") || version.contains("-dev") {
        println!("cargo:rustc-cfg=detcore_testutils_nightly");
    }
}
