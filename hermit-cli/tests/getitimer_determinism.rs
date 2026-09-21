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
fn getitimer_tracks_logical_alarm_state() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let source = repository.join("tests/c/getitimer_determinism_probe.c");
    let guest = Path::new(env!("CARGO_TARGET_TMPDIR")).join("getitimer-determinism-probe");

    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile getitimer probe");
    assert!(
        compile.status.success(),
        "probe compilation failed: {}",
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
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(guest)
        .output()
        .expect("failed to run getitimer probe");
    let stdout = String::from_utf8_lossy(&verify.stdout);
    let stderr = String::from_utf8_lossy(&verify.stderr);
    assert!(
        verify.status.success(),
        "strict verification failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        verify.status
    );
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "Hermit omitted its determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
