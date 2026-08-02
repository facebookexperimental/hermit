/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

static LITEINST_RUNTIME: OnceLock<()> = OnceLock::new();

pub(super) fn hermit_binary() -> PathBuf {
    std::env::var_os("HERMIT_LITEINST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hermit")))
}

pub(super) fn liteinst_runtime_library() -> PathBuf {
    hermit_binary()
        .parent()
        .expect("Hermit test binary should have a profile directory")
        .join("libreverie_liteinst.so")
}

pub(super) fn ensure_liteinst_runtime() {
    LITEINST_RUNTIME.get_or_init(|| {
        let hermit = hermit_binary();
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
        let runtime_target = target_dir.join("liteinst-runtime-build-028fe523");
        let runtime = liteinst_runtime_library();
        let output = Command::new(repository.join("scripts/stage-liteinst-runtime.sh"))
            .current_dir(repository)
            .arg(cargo_profile)
            .arg(&runtime)
            .arg(&runtime_target)
            .output()
            .expect("failed to build the LiteInst runtime");
        assert!(
            output.status.success(),
            "LiteInst runtime build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            runtime.is_file(),
            "standalone LiteInst runtime build did not stage {}",
            runtime.display(),
        );
    });
}
