/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end determinism coverage for an io_uring ring-driving guest.
//!
//! `tests/c/io_uring_ring_determinism.c` sets up a real io_uring submission and
//! completion queue with raw syscalls (no liburing), submits an
//! `IORING_OP_WRITE`, reaps the completion, reads the data back, and folds it
//! into an FNV-1a checksum. Hermit currently returns `ENOSYS` for
//! `io_uring_setup` (see `io_uring_fallback.c`), so under Hermit this guest
//! takes its deterministic "unsupported" branch instead. Either way the guest
//! prints a stable payload and the `io_uring_ring success` marker, so this test
//! is a determinism witness for the io_uring path — including the fallback that
//! real async runtimes rely on — rather than a passthrough assertion.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

const RUNS: usize = 5;
const SUCCESS_MARKER: &str = "io_uring_ring success\n";

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static IO_URING_RING_GUEST: OnceLock<PathBuf> = OnceLock::new();

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

fn io_uring_ring_guest() -> &'static Path {
    IO_URING_RING_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("io-uring-ring-determinism");
        fs::create_dir_all(&build_root)
            .expect("failed to create io_uring-ring guest build directory");
        let output = build_root.join("io_uring_ring_determinism");

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
            .arg(repository.join("tests/c/io_uring_ring_determinism.c"))
            .arg("-o")
            .arg(&output);
        command_output(command, "io_uring-ring guest compilation");
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
        .arg(io_uring_ring_guest());

    let output = command_output(command, &format!("io_uring-ring run {run}/{RUNS}"));
    assert!(
        output.stdout.ends_with(SUCCESS_MARKER.as_bytes()),
        "io_uring-ring omitted its success marker on run {run}/{RUNS}:\n{}",
        String::from_utf8_lossy(&output.stdout),
    );
    output.stdout
}

#[test]
fn io_uring_ring_payload_is_deterministic() {
    let _guard = hermit_run_lock();
    let expected = run_once(1);

    for run in 2..=RUNS {
        let actual = run_once(run);
        assert_eq!(
            actual,
            expected,
            "io_uring-ring output changed on run {run}/{RUNS}:\nexpected:\n{}actual:\n{}",
            String::from_utf8_lossy(&expected),
            String::from_utf8_lossy(&actual),
        );
    }
}

#[test]
#[ignore = "e2e: requires hermit + mount namespaces"]
fn io_uring_ring_reaches_strict_verify_l2() {
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
        .arg(io_uring_ring_guest());

    let output = command_output(command, "io_uring-ring strict verification");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "io_uring-ring exited 0 without Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
