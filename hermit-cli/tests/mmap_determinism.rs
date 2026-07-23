/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

const RUNS: usize = 5;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static MMAP_GUEST: OnceLock<PathBuf> = OnceLock::new();

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

fn mmap_guest() -> &'static Path {
    MMAP_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mmap-determinism");
        fs::create_dir_all(&build_root).expect("failed to create mmap guest build directory");
        let output = build_root.join("mmap_determinism");

        let mut command = Command::new("cc");
        command
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/mmap_determinism.c"))
            .arg("-o")
            .arg(&output);
        command_output(command, "mmap guest compilation");
        output
    })
}

fn run_scenario(scenario: &str, run: usize) -> Vec<u8> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ])
        .arg(mmap_guest())
        .arg(scenario);

    let output = command_output(command, &format!("{scenario} mmap run {run}/{RUNS}"));
    assert!(
        output.stdout.windows(2).any(|window| window == b"0x"),
        "{scenario} did not print an address: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    output.stdout
}

fn assert_scenario_is_deterministic(scenario: &str) {
    let _guard = hermit_run_lock();
    let expected = run_scenario(scenario, 1);

    for run in 2..=RUNS {
        let actual = run_scenario(scenario, run);
        assert_eq!(
            actual,
            expected,
            "{scenario} addresses changed on run {run}/{RUNS}:\nexpected: {}actual: {}",
            String::from_utf8_lossy(&expected),
            String::from_utf8_lossy(&actual),
        );
    }
}

#[test]
fn multiple_mmap_addresses_are_deterministic() {
    assert_scenario_is_deterministic("multiple");
}

#[test]
fn map_fixed_address_is_deterministic() {
    assert_scenario_is_deterministic("fixed");
}

#[test]
fn brk_and_sbrk_addresses_are_deterministic() {
    assert_scenario_is_deterministic("heap");
}

#[test]
fn map_shared_address_is_deterministic() {
    assert_scenario_is_deterministic("shared");
}

#[test]
fn mmap_reuses_unmapped_address_deterministically() {
    assert_scenario_is_deterministic("reuse");
}
