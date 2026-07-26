/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

static LITEINST_RUNTIME: OnceLock<()> = OnceLock::new();

pub(super) fn ensure_liteinst_runtime() {
    LITEINST_RUNTIME.get_or_init(|| {
        let hermit = Path::new(env!("CARGO_BIN_EXE_hermit"));
        let profile_dir = hermit
            .parent()
            .expect("Hermit test binary should have a profile directory");
        let profile = profile_dir
            .file_name()
            .expect("Hermit profile directory should have a name");
        let cargo_profile = if profile == OsStr::new("debug") {
            OsStr::new("dev")
        } else {
            profile
        };
        let target_dir = profile_dir
            .parent()
            .expect("Hermit profile should be inside a target directory");
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = Command::new(cargo)
            .current_dir(repository)
            .args(["build", "--locked", "-p", "detcore-liteinst", "--profile"])
            .arg(cargo_profile)
            .arg("--target-dir")
            .arg(target_dir)
            .output()
            .expect("failed to build the LiteInst runtime");
        assert!(
            output.status.success(),
            "LiteInst runtime build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let runtime = profile_dir.join("libdetcore_liteinst.so");
        assert!(
            runtime.is_file(),
            "detcore-liteinst build did not create {}",
            runtime.display(),
        );
    });
}
