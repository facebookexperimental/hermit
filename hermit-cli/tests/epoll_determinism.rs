/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::ffi::OsStr;
use std::ffi::OsString;
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
    hermit_test::configure_guest_execution(&mut command);
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
        // This integration target is selected by more than one DAG step. Each
        // nextest invocation is a separate process, so the lock and OnceLock
        // above cannot protect a shared output path across those invocations.
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("epoll-determinism-{}", std::process::id()));
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
    let guest = epoll_guest();
    let mut command = Command::new(hermit_test::hermit_binary());
    command.current_dir(
        guest
            .parent()
            .expect("epoll guest should have a build directory"),
    );
    command.args([
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    command.arg("--").arg(guest).arg(scenario);

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

fn assert_scenario_reaches_l2(scenario: &str) {
    let _guard = hermit_run_lock();
    let guest = epoll_guest();
    let mut command = Command::new("timeout");
    command
        .current_dir(
            guest
                .parent()
                .expect("epoll guest should have a build directory"),
        )
        .args(["--kill-after", "10s", "60s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "run",
            "--strict",
            "--verify",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
        ]);
    command.arg("--").arg(guest).arg(scenario);

    let output = command_output(command, &format!("{scenario} strict verification"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "{scenario} exited 0 without Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
#[ignore = "validate: fixed /test argument contract"]
fn pinned_root_arguments_are_exact_and_fail_closed() {
    assert_eq!(
        hermit_test::guest_args_for(["run", "--strict", "--", "/bin/true"], None,).unwrap(),
        ["run", "--strict", "--", "/bin/true"].map(OsString::from)
    );
    assert_eq!(
        hermit_test::guest_args_for(
            ["run", "--strict", "--", "/bin/true"],
            Some(OsStr::new("/test")),
        )
        .unwrap(),
        [
            "run",
            "--strict",
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
            "--",
            "/bin/true",
        ]
        .map(OsString::from)
    );
    assert_eq!(
        hermit_test::guest_args_for(
            [
                "run",
                "--backend=dbt",
                "--base-env=minimal",
                "--",
                "/bin/true",
            ],
            Some(OsStr::new("/test")),
        )
        .unwrap(),
        [
            "run",
            "--backend=dbt",
            "--base-env=minimal",
            "--workdir=/test",
            "--",
            "/bin/true",
        ]
        .map(OsString::from)
    );
    assert_eq!(
        hermit_test::guest_args_for(
            ["run", "--no-namespace", "--", "/bin/true"],
            Some(OsStr::new("/test")),
        )
        .unwrap(),
        [
            "run",
            "--no-namespace",
            "--base-env=minimal",
            "--workdir=/test",
            "--",
            "/bin/true",
        ]
        .map(OsString::from)
    );
    let error = hermit_test::guest_args_for(
        ["run", "--base-env=inherit", "--", "/bin/true"],
        Some(OsStr::new("/test")),
    )
    .unwrap_err();
    assert!(error.contains("requires --base-env=minimal"));

    let mut timeout = Command::new("timeout");
    timeout.arg("60s").arg(hermit_test::hermit_binary()).args([
        "run",
        "--strict",
        "--",
        "/bin/true",
    ]);
    hermit_test::configure_guest_execution_for(&mut timeout, Some(OsStr::new("/test"))).unwrap();
    assert_eq!(
        timeout
            .get_args()
            .map(OsStr::to_os_string)
            .collect::<Vec<_>>(),
        vec![
            OsString::from("60s"),
            hermit_test::hermit_binary().as_os_str().to_os_string(),
            OsString::from("run"),
            OsString::from("--strict"),
            OsString::from("--base-env=minimal"),
            OsString::from("--mount=type=tmpfs,target=/test"),
            OsString::from("--workdir=/test"),
            OsString::from("--"),
            OsString::from("/bin/true"),
        ]
    );

    let mut refused = Command::new(hermit_test::hermit_binary());
    refused.args(["run", "--", "/bin/true"]);
    let original = refused
        .get_args()
        .map(OsStr::to_os_string)
        .collect::<Vec<_>>();
    let error = hermit_test::configure_guest_execution_for(&mut refused, Some(OsStr::new("/tmp")))
        .unwrap_err();
    assert!(error.contains("HERMIT_E2E_EMPTY_WORKDIR must be /test"));
    assert_eq!(
        refused
            .get_args()
            .map(OsStr::to_os_string)
            .collect::<Vec<_>>(),
        original
    );

    let error = hermit_test::guest_args_for(["run", "--", "/bin/true"], Some(OsStr::new("/tmp")))
        .unwrap_err();
    assert!(error.contains("HERMIT_E2E_EMPTY_WORKDIR must be /test"));
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

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#549)
#[test]
fn notification_control_syscalls_are_deterministic() {
    assert_scenario_is_deterministic("control-fds");
}

#[test]
#[ignore = "e2e: requires hermit + mount namespaces"]
fn notification_control_syscalls_reach_strict_verify_l2() {
    assert_scenario_reaches_l2("control-fds");
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
