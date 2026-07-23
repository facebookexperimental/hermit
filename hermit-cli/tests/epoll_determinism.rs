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
static EPOLL_GUEST: OnceLock<PathBuf> = OnceLock::new();

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

fn epoll_guest() -> &'static Path {
    EPOLL_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("epoll-determinism");
        fs::create_dir_all(&build_root).expect("failed to create epoll guest build directory");
        let output = build_root.join("epoll_determinism");

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
            .arg(repository.join("tests/c/epoll_determinism.c"))
            .arg("-o")
            .arg(&output);
        command_output(command, "epoll guest compilation");
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
        .arg(epoll_guest())
        .arg(scenario);

    let output = command_output(command, &format!("{scenario} epoll run {run}/{RUNS}"));
    let expected_success = format!("{scenario} success\n");
    assert!(
        output.stdout.ends_with(expected_success.as_bytes()),
        "{scenario} omitted its success marker:\n{}",
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
            "{scenario} event ordering changed on run {run}/{RUNS}:\nexpected:\n{}actual:\n{}",
            String::from_utf8_lossy(&expected),
            String::from_utf8_lossy(&actual),
        );
    }
}

#[test]
fn multiple_ready_fds_have_deterministic_ordering() {
    assert_scenario_is_deterministic("multi");
}

#[test]
fn edge_triggered_delivery_is_deterministic() {
    assert_scenario_is_deterministic("edge");
}

#[test]
fn oneshot_delivery_and_rearming_are_deterministic() {
    assert_scenario_is_deterministic("oneshot");
}

#[test]
fn mixed_fd_readiness_is_deterministic() {
    assert_scenario_is_deterministic("mixed");
}

#[test]
fn nested_epoll_delivery_is_deterministic() {
    assert_scenario_is_deterministic("nested");
}

/// Regression test: descriptor-table operations on an epoll fd (F_GETFL,
/// F_SETFD, dup, F_DUPFD, F_DUPFD_CLOEXEC) used to fail with EBADF under Hermit
/// because the epoll fd was never registered in Detcore's fd table. This broke
/// the rustup proxies (cargo/rustc), whose tokio runtime dups its epoll fd at
/// startup. The guest aborts with a nonzero status if any operation fails, so a
/// successful run (asserted by `run_scenario`) is the regression check.
#[test]
fn epoll_fd_supports_descriptor_table_ops() {
    let _guard = hermit_run_lock();
    let output = run_scenario("dupfd", 1);
    assert!(
        output
            .windows(b"dupfd ops-ok".len())
            .any(|window| window == b"dupfd ops-ok"),
        "dupfd scenario did not report ops-ok:\n{}",
        String::from_utf8_lossy(&output),
    );
}
