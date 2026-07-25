/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression test for GH #81: chaos mode must not starve sched_yield loops
//! when timer preemption is disabled.
//!
//! Before the fix, a guest whose main thread spins on `sched_yield()` while
//! waiting for a worker thread could hang forever under
//! `--chaos --preemption-timeout=disabled`: priorities are fixed at thread
//! creation and only re-randomized at (now-disabled) timer preemptions, so a
//! spinner holding the highest priority monopolized the single logical CPU. The
//! seeds exercised below deterministically reproduced that starvation. The fix
//! turns `sched_yield` into a chaos reprioritization point, so every seed now
//! makes progress and exits cleanly.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// Chaos seeds that deterministically starved the sched_yield loop before the
/// fix (verified by bisecting the unfixed binary). Any of these hanging is a
/// regression.
const SEEDS: [u64; 4] = [5, 6, 9, 12];

/// Generous per-run timeout. A healthy run finishes in well under a second; a
/// starved run would otherwise spin forever.
const TIMEOUT_SECONDS: u64 = 30;

static GUEST: OnceLock<PathBuf> = OnceLock::new();

fn guest() -> &'static Path {
    GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root =
                Path::new(env!("CARGO_TARGET_TMPDIR")).join("chaos-sched-yield-progress");
            fs::create_dir_all(&build_root).expect("failed to create build directory");
            let output = build_root.join("sched_yield_progress");
            let mut command = Command::new("cc");
            command
                .args([
                    "-std=c11",
                    "-O2",
                    "-g",
                    "-pthread",
                    "-D_GNU_SOURCE",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/sched_yield_progress.c"))
                .arg("-o")
                .arg(&output);
            let status = command
                .status()
                .expect("failed to run cc to build sched_yield_progress guest");
            assert!(status.success(), "guest compilation failed: {command:?}");
            output
        })
        .as_path()
}

fn run_seed(seed: u64) {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--chaos",
            "--preemption-timeout=disabled",
            &format!("--seed={seed}"),
            "--",
        ])
        .arg(guest());

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start guest (seed {seed}): {rendered}: {error}"));

    // `timeout` exits 124 when it has to kill the child; that is exactly the
    // starvation symptom this test guards against.
    assert_ne!(
        output.status.code(),
        Some(124),
        "sched_yield loop starved (timed out) under chaos with seed {seed}: {rendered}"
    );
    assert!(
        output.status.success(),
        "guest failed under chaos with seed {seed}: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8(output.stdout).expect("guest stdout should be UTF-8");
    assert!(
        stdout.contains("sched-yield-progress-ok"),
        "missing progress marker under chaos with seed {seed}; stdout:\n{stdout}"
    );
}

#[test]
fn chaos_sched_yield_makes_progress_without_timer_preemption() {
    for seed in SEEDS {
        run_seed(seed);
    }
}

fn run_strict_guest(args: &[&str]) {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--strict",
            "--verify",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--",
        ])
        .arg(guest())
        .args(args);

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start strict guest: {rendered}: {error}"));

    assert_ne!(
        output.status.code(),
        Some(124),
        "sched_yield guest timed out under strict verify: {rendered}"
    );
    assert!(
        output.status.success(),
        "sched_yield guest failed under strict verify: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn strict_sched_yield_is_deterministic() {
    run_strict_guest(&[]);
}

#[test]
fn strict_vfork_child_sched_yield_is_deterministic() {
    run_strict_guest(&["--vfork"]);
}

#[test]
fn preemption_replay_preserves_vfork_sched_yield_progress() {
    let schedule = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sched-yield-preemptions.json");
    let _ = fs::remove_file(&schedule);

    for (phase, option) in [
        (
            "record",
            format!("--record-preemptions-to={}", schedule.display()),
        ),
        (
            "replay",
            format!("--replay-preemptions-from={}", schedule.display()),
        ),
    ] {
        let mut command = Command::new("timeout");
        command
            .arg("--kill-after=2s")
            .arg(format!("{TIMEOUT_SECONDS}s"))
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "run",
                "--strict",
                "--preemption-timeout=disabled",
                &option,
                "--base-env=minimal",
                "--no-virtualize-cpuid",
                "--",
            ])
            .arg(guest())
            .arg("--vfork");

        let rendered = format!("{command:?}");
        let output = command.output().unwrap_or_else(|error| {
            panic!("failed to start preemption {phase}: {rendered}: {error}")
        });
        assert_ne!(
            output.status.code(),
            Some(124),
            "preemption {phase} timed out: {rendered}"
        );
        assert!(
            output.status.success(),
            "preemption {phase} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
