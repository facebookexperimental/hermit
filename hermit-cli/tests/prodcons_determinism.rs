/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end determinism coverage for a blocking producer/consumer guest.
//!
//! `tests/c/prodcons_determinism.c` runs several producer and consumer threads
//! against a fixed-capacity ring guarded by a mutex and two condition variables
//! (not-full / not-empty). Unlike a spin-contention probe, the threads block on
//! the condition variables, so Hermit must serialize the futex-backed
//! mutex/condvar wakeups deterministically. The guest folds every consumed
//! payload into an order-independent checksum and verifies the produced and
//! consumed counts, then prints the counts and the `prodcons success` marker,
//! so a stable stdout is the determinism witness.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

const RUNS: usize = 5;
const SUCCESS_MARKER: &str = "prodcons success\n";

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static PRODCONS_GUEST: OnceLock<PathBuf> = OnceLock::new();

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

fn prodcons_guest() -> &'static Path {
    PRODCONS_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("prodcons-determinism");
        fs::create_dir_all(&build_root).expect("failed to create prodcons guest build directory");
        let output = build_root.join("prodcons_determinism");

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
                "-pthread",
            ])
            .arg(repository.join("tests/c/prodcons_determinism.c"))
            .arg("-o")
            .arg(&output);
        command_output(command, "prodcons guest compilation");
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
        .arg(prodcons_guest());

    let output = command_output(command, &format!("prodcons run {run}/{RUNS}"));
    assert!(
        output.stdout.ends_with(SUCCESS_MARKER.as_bytes()),
        "prodcons omitted its success marker on run {run}/{RUNS}:\n{}",
        String::from_utf8_lossy(&output.stdout),
    );
    output.stdout
}

#[test]
fn prodcons_counts_and_checksum_are_deterministic() {
    let _guard = hermit_run_lock();
    let expected = run_once(1);

    for run in 2..=RUNS {
        let actual = run_once(run);
        assert_eq!(
            actual,
            expected,
            "prodcons output changed on run {run}/{RUNS}:\nexpected:\n{}actual:\n{}",
            String::from_utf8_lossy(&expected),
            String::from_utf8_lossy(&actual),
        );
    }
}

#[test]
#[ignore = "e2e: requires hermit + mount namespaces"]
fn prodcons_reaches_strict_verify_l2() {
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
        .arg(prodcons_guest());

    let output = command_output(command, "prodcons strict verification");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "prodcons exited 0 without Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
