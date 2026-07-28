/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::process::Command;

#[test]
fn dumpability_controls_are_deterministic() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("prctl-dumpable");
    std::fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let guest = build_root.join("prctl-dumpable");

    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/prctl_dumpable.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile prctl dumpable guest");
    assert!(
        compile.status.success(),
        "guest compilation failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let verify = Command::new("timeout")
        .args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--backend=ptrace",
            "--strict",
            "--verify",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .output()
        .expect("failed to run prctl dumpable guest");
    let stdout = String::from_utf8_lossy(&verify.stdout);
    let stderr = String::from_utf8_lossy(&verify.stderr);
    assert!(
        verify.status.success(),
        "strict verification failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        verify.status
    );
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "Hermit omitted determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
