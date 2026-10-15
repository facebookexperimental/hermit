/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::process::Command;

// A strict run with a 1000-branch timeslice spends about 9.5 s starting the
// guest whatever its round count, and 500 rounds add about 7.5 s, so one
// verified pair of 100-round runs stays well inside the test's wall budget.
const ROUNDS: &str = "100";
const EXPECTED_STDOUT: &str = "sigmask-stress threads=4 rounds=100 checksum=03603a8086601e0c\n";

fn compile_guest() -> std::path::PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sigmask-preemption");
    fs::create_dir_all(&build_root).expect("failed to create sigmask build directory");
    let binary = build_root.join("sigmask_preemption");
    let output = Command::new("cc")
        .args([
            "-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror", "-pthread",
        ])
        .arg(repository.join("tests/c/sigmask_preemption.c"))
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("failed to compile sigmask preemption guest");
    assert!(
        output.status.success(),
        "sigmask guest compilation failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    binary
}

/// Every signal-mask change is an injected syscall, and a 1000-branch
/// timeslice preempts the guest throughout them. The test checks that the
/// guest still sees its masks change, and that two runs agree under the
/// canonical comparison.
///
/// Measured 2026-10-14 at Reverie 9e0ce576: replacing these injections'
/// `inject_with_retry` with `inject` again (the state before
/// https://github.com/rrnewton/hermit/pull/1046) no longer fails, at 20 to
/// 500 rounds, so this is not a regression test for that change today.
#[test]
fn interrupted_signal_mask_injections_are_retried() {
    let binary = compile_guest();
    let program = binary.to_str().expect("guest path should be UTF-8");

    let verified = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--strict",
            "--verify",
            "--verify-strict",
            "--epoch=2026-01-01T00:00:00Z",
            "--max-timeslice=1000",
            "--",
            program,
            ROUNDS,
        ])
        .output()
        .expect("failed to verify sigmask preemption guest");
    assert!(
        verified.status.success(),
        "sigmask verification failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&verified.stdout),
        String::from_utf8_lossy(&verified.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&verified.stdout), EXPECTED_STDOUT);
    let stderr = String::from_utf8_lossy(&verified.stderr);
    assert!(
        stderr.contains("comparison=BitwiseInfoV1 relaxations=none")
            && stderr.contains("Determinism verified"),
        "sigmask verification omitted the canonical success markers:\n{stderr}",
    );
}
