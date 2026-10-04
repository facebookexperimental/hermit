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

const EXPECTED_STDOUT: &str = "sigmask-stress threads=4 rounds=500 checksum=f183820163e0384c\n";

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

#[test]
fn interrupted_signal_mask_injections_are_retried() {
    let binary = compile_guest();
    let program = binary.to_str().expect("guest path should be UTF-8");

    let strict = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--strict", "--max-timeslice=1000", "--", program])
        .output()
        .expect("failed to run strict sigmask preemption guest");
    assert!(
        strict.status.success(),
        "strict sigmask guest failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&strict.stdout),
        String::from_utf8_lossy(&strict.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&strict.stdout), EXPECTED_STDOUT);

    let verified = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--strict",
            "--verify",
            "--max-timeslice=1000",
            "--",
            program,
        ])
        .output()
        .expect("failed to verify sigmask preemption guest");
    assert!(
        verified.status.success(),
        "sigmask verification failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&verified.stdout),
        String::from_utf8_lossy(&verified.stderr),
    );
    assert!(
        String::from_utf8_lossy(&verified.stderr).contains("Determinism verified"),
        "sigmask verification omitted success marker:\n{}",
        String::from_utf8_lossy(&verified.stderr),
    );
}
