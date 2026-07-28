/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! AUTONOMOUS-BOT-IMPLEMENTED
//! TODO-HUMAN-REVIEW(PR-973): Review positioned/copy procfs bypass coverage.
//!
//! Regression coverage for the systemic procfs determinism gap where only the
//! sequential `read` path consumed the sanitized [`ProcfsFile`] snapshot while
//! `pread64` and `sendfile` read live kernel bytes. The probe asserts that a
//! positioned read observes the sanitized snapshot and that a procfs `sendfile`
//! input is refused, and Hermit's own `--verify` proves both are deterministic.

use std::path::Path;
use std::process::Command;

#[test]
fn procfs_positioned_reads_are_mediated_and_deterministic() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let source = repository.join("tests/c/procfs_positioned_probe.c");
    let guest = Path::new(env!("CARGO_TARGET_TMPDIR")).join("procfs-positioned-probe");

    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile procfs positioned probe");
    assert!(
        compile.status.success(),
        "probe compilation failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    // First a plain strict run so the guest's stdout reaches this process. The
    // probe itself asserts that pread observed the *sanitized* snapshot (stat
    // starttime == 0) and that a procfs sendfile input was refused; a nonzero
    // guest exit fails `status.success()`, and the markers confirm both checks
    // actually executed rather than being skipped.
    let run = Command::new("timeout")
        .args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--backend=ptrace",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .output()
        .expect("failed to run procfs positioned probe");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        run.status.success(),
        "strict run failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        run.status
    );
    assert!(
        stdout.contains("procfs-pread-sanitized-ok"),
        "pread did not observe the sanitized snapshot\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("procfs-sendfile-refused-ok"),
        "sendfile with a procfs input was not refused\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Then a --verify run so Hermit proves the positioned/copy paths are
    // bitwise identical across executions (they would diverge if pread read
    // live kernel bytes).
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
        .arg(&guest)
        .output()
        .expect("failed to verify procfs positioned probe");
    let vstdout = String::from_utf8_lossy(&verify.stdout);
    let vstderr = String::from_utf8_lossy(&verify.stderr);
    assert!(
        verify.status.success(),
        "strict verification failed: {}\nstdout:\n{vstdout}\nstderr:\n{vstderr}",
        verify.status
    );
    assert!(
        vstdout.contains("Determinism verified") || vstderr.contains("Determinism verified"),
        "Hermit omitted its determinism marker\nstdout:\n{vstdout}\nstderr:\n{vstderr}"
    );
}
