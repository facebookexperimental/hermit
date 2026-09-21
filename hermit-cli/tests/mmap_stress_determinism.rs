/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end determinism coverage for an mmap-heavy guest.
//!
//! `tests/c/mmap_stress_determinism.c` allocates many anonymous maps of varied
//! sizes, touches their pages deterministically, toggles `mprotect`
//! protections, grows and shrinks a region with `mremap`, and drives a
//! `MAP_SHARED` anonymous region through `msync`, then folds every touched byte
//! into an FNV-1a checksum. Because Hermit fixes the guest address layout, both
//! the addresses and the resulting checksum must be identical across runs; the
//! guest prints the checksum and the `mmap_stress success` marker so a stable
//! stdout is the determinism witness.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

const RUNS: usize = 5;
const SUCCESS_MARKER: &str = "mmap_stress success\n";

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static MMAP_STRESS_GUEST: OnceLock<PathBuf> = OnceLock::new();

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn mmap_stress_guest() -> &'static Path {
    MMAP_STRESS_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mmap-stress-determinism");
        fs::create_dir_all(&build_root)
            .expect("failed to create mmap-stress guest build directory");
        let output = build_root.join("mmap_stress_determinism");

        let mut command = Command::new("cc");
        command
            .args([
                "-O0",
                "-g",
                "-D_GNU_SOURCE",
                "-std=c11",
                "-Wall",
                "-Wextra",
                "-Werror",
            ])
            .arg(repository.join("tests/c/mmap_stress_determinism.c"))
            .arg("-o")
            .arg(&output);
        command_output(command, "mmap-stress guest compilation");
        output
    })
}

fn run_once(run: usize) -> Vec<u8> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--",
        ])
        .arg(mmap_stress_guest());

    let output = command_output(command, &format!("mmap-stress run {run}/{RUNS}"));
    assert!(
        output.stdout.ends_with(SUCCESS_MARKER.as_bytes()),
        "mmap-stress omitted its success marker on run {run}/{RUNS}:\n{}",
        String::from_utf8_lossy(&output.stdout),
    );
    output.stdout
}

#[test]
fn mmap_stress_layout_and_checksum_are_deterministic() {
    let _guard = hermit_run_lock();
    let expected = run_once(1);

    for run in 2..=RUNS {
        let actual = run_once(run);
        assert_eq!(
            actual,
            expected,
            "mmap-stress output changed on run {run}/{RUNS}:\nexpected:\n{}actual:\n{}",
            String::from_utf8_lossy(&expected),
            String::from_utf8_lossy(&actual),
        );
    }
}

#[test]
#[ignore = "e2e: requires hermit + mount namespaces"]
fn mmap_stress_reaches_strict_verify_l2() {
    let _guard = hermit_run_lock();
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "120s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--strict",
            "--verify",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ])
        .arg(mmap_stress_guest());

    let output = command_output(command, "mmap-stress strict verification");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "mmap-stress exited 0 without Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
