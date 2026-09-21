/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression coverage for classifying KVM host lifecycle telemetry.
//!
//! KVM's built-in `--verify` path compares exit status/stdout/stderr only; it
//! does not compare internal INFO logs and cannot establish L2. This test does
//! not change that. It uses the separate, standalone `hermit log-diff` command
//! to inspect one captured log through the production canonical INFO extractor,
//! then checks that host wall-clock timings are excluded there but retained at
//! DEBUG for profiling.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

static KVM_RUN_LOCK: Mutex<()> = Mutex::new(());

const LIFECYCLE_TIMING_EVENT: &str = "reverie-kvm lifecycle phase timings";

fn compile_guest() -> PathBuf {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-lifecycle-timing-log-level");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let source = build_root.join("guest.c");
    fs::write(&source, "int main(void) { return 0; }\n").expect("failed to write guest source");
    let binary = build_root.join("guest");
    let output = Command::new("cc")
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap_or_else(|error| panic!("failed to compile guest: {error}"));
    assert!(
        output.status.success(),
        "failed to compile guest:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    binary
}

fn run_kvm(binary: &Path, log_level: &str) -> String {
    let output = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log", log_level, "--backend", "kvm"])
        .args(["run", "--strict", "--"])
        .arg(binary)
        .env_remove("HERMIT_LOG")
        .env_remove("HERMIT_LOG_FILE")
        .env_remove("RUST_LOG")
        .output()
        .unwrap_or_else(|error| panic!("failed to run guest under KVM: {error}"));
    assert!(
        output.status.success(),
        "KVM strict run at {log_level} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn canonical_info_from_standalone_log_diff(log: &str) -> String {
    let directory = tempfile::tempdir().expect("failed to create temporary log directory");
    let log_path = directory.path().join("kvm-info.log");
    fs::write(&log_path, log).expect("failed to write KVM log");

    // One-input log-diff prints the production canonical INFO stream. This is
    // an offline inspection step, not KVM's output/status-only built-in verify.
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .arg("log-diff")
        .arg("--record-envelope=all-records-v1")
        .arg(&log_path)
        .env_remove("HERMIT_LOG")
        .env_remove("HERMIT_LOG_FILE")
        .env_remove("RUST_LOG")
        .output()
        .unwrap_or_else(|error| panic!("failed to inspect KVM INFO log: {error}"));
    assert!(
        output.status.success(),
        "standalone canonical INFO extraction failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("canonical INFO stream was not UTF-8")
}

#[test]
fn kvm_lifecycle_timings_are_debug_only() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP kvm_lifecycle_timings_are_debug_only: /dev/kvm is not present");
        return;
    }
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let binary = compile_guest();
    let info_log = run_kvm(&binary, "info");
    let debug_log = run_kvm(&binary, "debug");
    let canonical_info = canonical_info_from_standalone_log_diff(&info_log);

    assert!(
        canonical_info.contains("launching guest through reverie-kvm"),
        "canonical INFO extraction did not prove that the KVM path emitted its launch telemetry",
    );
    assert!(
        canonical_info.lines().any(|line| {
            line.contains("INFO detcore: DETLOG")
                || (line.contains("INFO detcore::scheduler:") && line.contains("COMMIT turn"))
        }),
        "canonical INFO extraction contained no Detcore DETLOG or scheduler COMMIT evidence",
    );
    assert!(
        !canonical_info.contains(LIFECYCLE_TIMING_EVENT),
        "host lifecycle timings leaked into the canonical INFO stream",
    );

    let timing_lines = debug_log
        .lines()
        .filter(|line| line.contains(LIFECYCLE_TIMING_EVENT))
        .collect::<Vec<_>>();
    assert_eq!(
        timing_lines.len(),
        1,
        "expected exactly one DEBUG lifecycle timing event; captured: {timing_lines:?}",
    );
    for field in [
        "prepare_us=",
        "setup_us=",
        "execution_us=",
        "cleanup_us=",
        "teardown_us=",
        "lifecycle_us=",
    ] {
        assert!(
            timing_lines[0].contains(field),
            "DEBUG lifecycle timing event is missing {field}: {}",
            timing_lines[0],
        );
    }
}
