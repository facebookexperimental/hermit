/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Output;

fn compile_guest(output: &Path) {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).expect("failed to create random guest build directory");
    }
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let result = Command::new("cc")
        .args(["-O2", "-g", "-pthread", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/random_sources.c"))
        .arg("-o")
        .arg(output)
        .output()
        .expect("failed to start random guest compilation");
    assert!(
        result.status.success(),
        "random guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

/// One `hermit run` of the guest with `options` before it.
fn hermit_run(guest: &Path, options: &[String]) -> Output {
    let mut command = Command::new(hermit_test::hermit_binary());
    command
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
        ])
        .args(options)
        .arg(guest);
    hermit_test::configure_guest_execution(&mut command);
    hermit_test::hermit_output(&format!("{command:?}"), &mut command)
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "random guest failed for {label}:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_guest(guest: &Path, seed: u64) -> Vec<u8> {
    let output = hermit_run(guest, &[format!("--rng-seed={seed}")]);
    assert_success(&output, &format!("seed {seed}"));
    output.stdout
}

fn assert_guest_l2(guest: &Path, backend: Option<&str>) {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "60s"])
        .arg(hermit_test::hermit_binary())
        .arg("--log=info");
    if let Some(backend) = backend {
        command.args(["--backend", backend]);
    }
    command.arg("run");
    command
        .args([
            "--strict",
            "--verify",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ])
        .arg(guest);
    hermit_test::configure_guest_execution(&mut command);
    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "random guest did not reach L2 under strict verification: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "random guest exited 0 without Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn random_sources_repeat_across_runs_and_change_with_seed() {
    let guest = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "random-determinism/random-sources-{}",
        std::process::id()
    ));
    compile_guest(&guest);

    let expected = run_guest(&guest, 17);
    assert!(!expected.is_empty());
    for _ in 1..5 {
        assert_eq!(run_guest(&guest, 17), expected);
    }
    assert_ne!(run_guest(&guest, 18), expected);
}

/// A schedule replay hands the guest the random values its recording read.
/// `--seed` seeds the guest's randomness as well as the scheduler, so a
/// preemption record stores the run's `--rng-seed`. A replay that omits
/// `--rng-seed` adopts it, under either replay flag, and a different explicit
/// one is refused before the guest runs.
#[test]
fn a_schedule_replay_hands_the_guest_the_recorded_random_values() {
    let guest = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "random-determinism/random-sources-replay-{}",
        std::process::id()
    ));
    compile_guest(&guest);
    let schedule = guest.with_extension("preemptions.json");
    let recorded = hermit_run(
        &guest,
        &[
            "--seed=17".to_owned(),
            format!("--record-preemptions-to={}", schedule.display()),
        ],
    );
    assert_success(&recorded, "the recording at --seed=17");
    let unseeded = hermit_run(&guest, &[]);
    assert_success(&unseeded, "the default seed");
    assert_ne!(
        recorded.stdout, unseeded.stdout,
        "the premise: the default seed hands the guest other values"
    );

    for flag in ["--replay-preemptions-from", "--replay-schedule-from"] {
        let replayed = hermit_run(&guest, &[format!("{flag}={}", schedule.display())]);
        assert_success(&replayed, flag);
        assert_eq!(
            String::from_utf8_lossy(&replayed.stdout),
            String::from_utf8_lossy(&recorded.stdout),
            "{flag} handed the guest other random values than the recording"
        );
    }

    let refused = hermit_run(
        &guest,
        &[
            "--rng-seed=18".to_owned(),
            format!("--replay-preemptions-from={}", schedule.display()),
        ],
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    // EXIT-CLASS: hermit
    assert_eq!(refused.status.code(), Some(122), "{stderr}");
    assert!(
        stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
        "{stderr}"
    );
    assert!(
        stderr.contains("--rng-seed=18 differs from the seed 17"),
        "{stderr}"
    );
    assert!(
        refused.stdout.is_empty(),
        "the guest ran despite the refusal: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
}

#[test]
#[ignore = "e2e: requires hermit + mount namespaces"]
fn random_sources_are_deterministic_under_strict_verify() {
    let guest = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "random-determinism/random-sources-strict-{}",
        std::process::id()
    ));
    compile_guest(&guest);
    assert_guest_l2(&guest, None);
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1060): Review DBT child identity and RNG lifecycle coverage.
#[test]
fn dbt_random_sources_are_deterministic_under_strict_verify() {
    let guest = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "random-determinism/dbt-random-sources-{}",
        std::process::id()
    ));
    compile_guest(&guest);
    assert_guest_l2(&guest, Some("dbt"));
}
