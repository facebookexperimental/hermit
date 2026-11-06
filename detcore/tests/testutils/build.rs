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
//! `detcore_testutils_nightly` only on a nightly/dev compiler (or a stable
//! compiler run with RUSTC_BOOTSTRAP), so stable builds compile the capture
//! code out exactly as they do with the feature off.

use std::process::Command;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(detcore_testutils_nightly)");
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=RUSTC_BOOTSTRAP");

    if std::env::var_os("RUSTC_BOOTSTRAP").is_some() {
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
