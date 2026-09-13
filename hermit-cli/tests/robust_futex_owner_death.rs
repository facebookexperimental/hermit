/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression coverage for Linux's robust-futex owner-death protocol.
//!
//! `tests/bin/robust_futex_test.c` parks a waiter on a `PTHREAD_MUTEX_ROBUST`
//! mutex, sets `FUTEX_WAITERS`, and then lets the owning thread exit while still
//! holding the mutex. Linux marks the futex word `FUTEX_OWNER_DIED` and wakes one
//! waiter, which is how the waiter's `pthread_mutex_lock` returns `EOWNERDEAD`.
//!
//! Before Detcore modeled that protocol, the precise futex model parked the
//! waiter in the scheduler's own pool where the kernel's internal wake could not
//! reach it: the run hung and then aborted with
//! "Deadlock detected: thread(s) waiting on futex, but no runnable threads left".
//!
//! This test asserts full L2 canonical parity (`--verify-strict --verify-json`
//! with `bitwise_parity: true`), which is strictly stronger than the E2E
//! harness's `verify` mode: that mode runs plain `--verify` and therefore only
//! establishes the stripped comparison.
//!
//! # Why the guest's own `EOWNERDEAD` is not enough
//!
//! `PASS: robust mutex waiter received EOWNERDEAD` does not discriminate this
//! change on its own. The guest's robust list stays registered with the host
//! kernel, so when the dying thread's `exit` finally reaches Linux, Linux runs
//! its own `exit_robust_list()` and writes `FUTEX_OWNER_DIED` into the word
//! too. Detcore deliberately leaves that atomic transition to Linux on the
//! ptrace backend, so the line proves the transition but not the modeled wake.
//!
//! The assertions below therefore also require Detcore's own log records that
//! its wake reached exactly one modeled waiter, the ptrace path deferred the
//! transition, and it did not execute the production store. The unit tests
//! separately expose why a backend without atomic or post-exit support must not
//! run the separate read/write path.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

/// Captured child output, always via files rather than pipes.
struct Captured {
    stdout: String,
    stderr: String,
}

/// Run `command` to completion, capturing its streams into `stdout_path` and
/// `stderr_path`.
///
/// Deliberately *not* `Command::output()`. When a Hermit run wedges, its
/// scheduler panics but leaves the guest in a ptrace stop, and `timeout` kills
/// only Hermit itself. An orphaned stopped tracee keeps an inherited pipe open
/// forever, so `output()` would block long past the `timeout` bound and turn a
/// clean regression failure into a hung test. Redirecting to files means the
/// wait ends when `timeout` exits, whatever the tracee does.
fn run_captured(
    mut command: Command,
    label: &str,
    stdout_path: &Path,
    stderr_path: &Path,
    expect_success: bool,
) -> Captured {
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let out = File::create(stdout_path).expect("failed to create stdout capture");
    let err = File::create(stderr_path).expect("failed to create stderr capture");
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .status()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    let captured = Captured {
        stdout: fs::read_to_string(stdout_path).unwrap_or_default(),
        stderr: fs::read_to_string(stderr_path).unwrap_or_default(),
    };
    if expect_success {
        assert!(
            status.success(),
            "{label} failed: {rendered}\nstatus: {status}\nstdout:\n{}\nstderr:\n{}",
            captured.stdout,
            captured.stderr,
        );
    }
    captured
}

fn assert_nonempty_canonical_l2(report_path: &Path, stdout: &str, stderr: &str) {
    let report_bytes = fs::read(report_path).unwrap_or_else(|error| {
        panic!(
            "missing verification report {}: {error}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            report_path.display()
        )
    });
    let report: serde_json::Value =
        serde_json::from_slice(&report_bytes).expect("Hermit verification report is valid JSON");
    assert!(
        report["verdict"] == "matched"
            && report["verified"] == true
            && report["bitwise_parity"] == true
            && report["comparison"]["strictness"] == "canonical"
            && report["comparison"]["log_scope"] == "info"
            && report["comparison"]["full_trace"] == true
            && report["compared_log_messages"]["left"]
                .as_u64()
                .is_some_and(|count| count > 0)
            && report["compared_log_messages"]["right"]
                .as_u64()
                .is_some_and(|count| count > 0)
            && report["guest_exit_code"] == 0,
        "verification report was not nonempty canonical bitwise parity: {report}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
    );
}

fn build_guest(build_root: &Path) -> PathBuf {
    fs::create_dir_all(build_root).expect("failed to create guest build directory");
    let guest = build_root.join("robust_futex_test");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(repository().join("tests/bin/robust_futex_test.c"))
        .args(["-pthread", "-o"])
        .arg(&guest);
    run_captured(
        compile,
        "robust-futex guest compilation",
        &build_root.join("compile.out"),
        &build_root.join("compile.err"),
        true,
    );
    guest
}

fn build_lifecycle_guest(build_root: &Path) -> PathBuf {
    fs::create_dir_all(build_root).expect("failed to create lifecycle guest build directory");
    let guest = build_root.join("robust_futex_owner_lifecycle");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(repository().join("tests/c/robust_futex_owner_lifecycle.c"))
        .args(["-pthread", "-o"])
        .arg(&guest);
    run_captured(
        compile,
        "robust-futex lifecycle guest compilation",
        &build_root.join("compile-lifecycle.out"),
        &build_root.join("compile-lifecycle.err"),
        true,
    );
    guest
}

/// The native control must pass, otherwise the guest itself is broken and any
/// Hermit result would be meaningless.
#[test]
fn robust_futex_owner_death_passes_natively() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("robust-futex-native");
    let guest = build_guest(&build_root);

    let mut native = Command::new("timeout");
    native.args(["--kill-after", "5s", "30s"]).arg(&guest);
    let captured = run_captured(
        native,
        "native robust-futex control",
        &build_root.join("native.out"),
        &build_root.join("native.err"),
        true,
    );
    assert!(
        captured
            .stdout
            .contains("PASS: robust mutex waiter received EOWNERDEAD"),
        "native control did not report EOWNERDEAD\nstdout:\n{}",
        captured.stdout
    );
}

#[test]
fn robust_futex_owner_death_wakes_the_waiter_at_l2() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("robust-futex-owner-death");
    let guest = build_guest(&build_root);
    let verify_json = build_root.join("ptrace.verify.json");
    let _ = fs::remove_file(&verify_json);

    let mut verify = Command::new("timeout");
    verify
        .args(["--kill-after", "5s", "120s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "run",
            "--backend=ptrace",
            "--strict",
            "--tmp=/tmp",
            "--verify",
            "--verify-strict",
            "--verify-json",
        ])
        .arg(&verify_json)
        .args(["--base-env=minimal", "--"])
        .arg(&guest);
    let captured = run_captured(
        verify,
        "robust-futex owner-death strict verification",
        &build_root.join("verify.out"),
        &build_root.join("verify.err"),
        true,
    );

    let stdout = captured.stdout;
    let stderr = captured.stderr;
    assert!(
        stdout.contains("PASS: robust mutex waiter received EOWNERDEAD"),
        "the waiter did not observe EOWNERDEAD under Hermit\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // `--verify` alone prints an identical success line for the lossy stripped
    // comparison, so the structured, nonempty canonical record is the only
    // thing that distinguishes L2.
    assert_nonempty_canonical_l2(&verify_json, &stdout, &stderr);
}

/// Detcore must wake the waiter parked in its precise futex model, while Linux
/// supplies the atomic owner-word transition on ptrace.
///
/// `--verify` sends each of its two runs' logs to its own temporary file and
/// prints only the comparison to stderr, so this evidence needs its own plain
/// strict run. The transition record is at DEBUG because it carries a raw
/// guest address and INFO is the surface `--verify-strict` compares, so this
/// run raises the log level rather than the record's level. It is also the L1
/// leg of the same behavior; the L2 leg is asserted above.
#[test]
fn detcore_wakes_the_modeled_waiter_before_linux_finishes_exit() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("robust-futex-detcore-evidence");
    let guest = build_guest(&build_root);

    let mut run = Command::new("timeout");
    run.args(["--kill-after", "5s", "120s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=debug",
            "run",
            "--backend=ptrace",
            "--strict",
            "--tmp=/tmp",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest);
    let captured = run_captured(
        run,
        "robust-futex owner-death instrumented run",
        &build_root.join("run.out"),
        &build_root.join("run.err"),
        true,
    );
    let stdout = captured.stdout;
    let stderr = captured.stderr;

    assert!(
        stdout.contains("PASS: robust mutex waiter received EOWNERDEAD"),
        "the waiter did not observe EOWNERDEAD under Hermit\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Detcore's wake reached the modeled waiter. A count of 0 is the
    // pre-fix state exactly: a wake is issued, but not into the pool where the
    // precise futex model actually parked the waiter.
    assert!(
        stderr.contains("robust-list owner death woke 1 waiter(s)"),
        "Detcore's owner-death wake did not reach exactly one modeled waiter\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("leaving futex word") && stderr.contains("for backend exit cleanup"),
        "ptrace did not exercise the production deferred-write branch\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("robust-list owner death: futex word"),
        "ptrace performed the prohibited separate owner-word write\nstderr:\n{stderr}"
    );
}

#[test]
fn linux_wakes_process_shared_waiters_for_all_three_owner_death_paths() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("robust-futex-lifecycle-native");
    let guest = build_lifecycle_guest(&build_root);

    for mode in ["signal", "exit-group", "de-thread"] {
        let mut native = Command::new("timeout");
        native
            .args(["--kill-after", "5s", "30s"])
            .arg(&guest)
            .arg(mode);
        let captured = run_captured(
            native,
            &format!("native robust-futex {mode} control"),
            &build_root.join(format!("native-{mode}.out")),
            &build_root.join(format!("native-{mode}.err")),
            true,
        );
        assert!(
            captured.stdout.contains("PASS: waiter received EOWNERDEAD"),
            "native {mode} control did not wake its process-shared waiter\nstdout:\n{}\nstderr:\n{}",
            captured.stdout,
            captured.stderr,
        );
    }
}

#[test]
fn ptrace_wakes_after_guest_observed_fatal_signal_and_exit_group_cleanup() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("robust-futex-lifecycle-ptrace");
    let guest = build_lifecycle_guest(&build_root);

    for mode in ["signal", "exit-group"] {
        let verify_json = build_root.join(format!("ptrace-{mode}.verify.json"));
        let _ = fs::remove_file(&verify_json);
        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "120s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=info",
                "run",
                "--backend=ptrace",
                "--strict",
                "--tmp=/tmp",
                "--verify",
                "--verify-strict",
                "--verify-json",
            ])
            .arg(&verify_json)
            .args(["--base-env=minimal", "--"])
            .arg(&guest)
            .arg(mode);
        let verified = run_captured(
            verify,
            &format!("ptrace robust-futex {mode} strict verification"),
            &build_root.join(format!("ptrace-{mode}-verify.out")),
            &build_root.join(format!("ptrace-{mode}-verify.err")),
            true,
        );
        assert!(
            verified.stdout.contains("PASS: waiter received EOWNERDEAD"),
            "ptrace {mode} verification did not make progress after owner death\nstdout:\n{}\nstderr:\n{}",
            verified.stdout,
            verified.stderr,
        );
        assert_nonempty_canonical_l2(&verify_json, &verified.stdout, &verified.stderr);

        let mut run = Command::new("timeout");
        run.args(["--kill-after", "5s", "120s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=debug",
                "run",
                "--backend=ptrace",
                "--strict",
                "--tmp=/tmp",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest)
            .arg(mode);
        let captured = run_captured(
            run,
            &format!("ptrace robust-futex {mode} run"),
            &build_root.join(format!("ptrace-{mode}.out")),
            &build_root.join(format!("ptrace-{mode}.err")),
            true,
        );
        assert!(
            captured.stdout.contains("PASS: waiter received EOWNERDEAD"),
            "ptrace {mode} did not make progress after owner death\nstdout:\n{}\nstderr:\n{}",
            captured.stdout,
            captured.stderr,
        );
        assert!(
            captured
                .stderr
                .contains("staged robust-list owner-death wake until physical exit"),
            "ptrace {mode} did not exercise the pre-exit collection path\nstderr:\n{}",
            captured.stderr,
        );
        assert!(
            captured.stderr.lines().any(|line| {
                line.contains("robust-list owner death woke 1 waiter(s) on futex")
                    && line.contains("after physical exit")
            }),
            "ptrace {mode} did not wake exactly one modeled waiter after physical cleanup\nstderr:\n{}",
            captured.stderr,
        );
    }
}
