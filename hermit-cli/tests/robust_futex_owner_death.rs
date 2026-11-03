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
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("robust-futex-native-{}", std::process::id()));
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
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("robust-futex-owner-death-{}", std::process::id()));
    let guest = build_guest(&build_root);
    let verify_json = build_root.join("ptrace.verify.json");
    let _ = fs::remove_file(&verify_json);

    let mut verify = Command::new("timeout");
    verify
        .args(["--kill-after", "5s", "120s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "--backend=ptrace",
            "run",
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
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-detcore-evidence-{}",
        std::process::id()
    ));
    let guest = build_guest(&build_root);

    let mut run = Command::new("timeout");
    run.args(["--kill-after", "5s", "120s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=debug",
            "--backend=ptrace",
            "run",
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
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-lifecycle-native-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);

    for mode in [
        "signal",
        "exit-group",
        "de-thread",
        "owner-exec",
        "leader-exec",
        "thread-tgid-exec",
    ] {
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
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-lifecycle-ptrace-{}",
        std::process::id()
    ));
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
                "--backend=ptrace",
                "run",
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
                "--backend=ptrace",
                "run",
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

/// The lifecycle guest's SIGKILL modes, each with the number of robust
/// mutexes whose owner it kills. Another process kills the owner's process
/// while a waiter waits: with `kill` (`sigkill`), with a `tgkill` naming a
/// thread that is not the owner (`sigkill-thread`), with `pidfd_send_signal`
/// (`sigkill-pidfd`), with `kill(-pgid)` (`sigkill-group`), while the owner
/// holds the mutex only through its pending slot (`sigkill-pending`), while
/// the waiter is a thread of the killing process (`sigkill-killer-waits`), or
/// while two of the process's threads each own a mutex with a waiter
/// (`sigkill-two-owners`). Or the owner's process kills itself, from a thread
/// that is not the owner (`sigkill-self`), from the owner
/// (`sigkill-self-owner`), or from a thread that owns one mutex while a
/// sibling owns the other (`sigkill-self-both`).
const SIGKILL_MODES: [(&str, usize); 10] = [
    ("sigkill", 1),
    ("sigkill-thread", 1),
    ("sigkill-pidfd", 1),
    ("sigkill-group", 1),
    ("sigkill-pending", 1),
    ("sigkill-killer-waits", 1),
    ("sigkill-two-owners", 2),
    ("sigkill-self", 1),
    ("sigkill-self-owner", 1),
    ("sigkill-self-both", 2),
];

/// Linux walks a robust-mutex owner's list however the owner dies, a SIGKILL
/// included: the waiter's lock returns `EOWNERDEAD` in every SIGKILL mode.
#[test]
fn linux_wakes_the_waiter_when_a_sigkill_ends_the_owner() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-sigkill-native-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);
    for (mode, owners) in SIGKILL_MODES {
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
            captured.stdout.contains("PASS: waiter received EOWNERDEAD")
                && (owners == 1
                    || captured
                        .stdout
                        .contains("PASS: second waiter received EOWNERDEAD")),
            "native {mode} control did not wake its waiters\nstdout:\n{}\nstderr:\n{}",
            captured.stdout,
            captured.stderr,
        );
    }
}

/// A SIGKILL ends its victims without a stop at which Detcore could read their
/// robust lists, so the sender records them before the send and each victim's
/// exit callback replays Linux's walk over its record. Each SIGKILL mode
/// passes at L2 on ptrace, and a strict run records one modeled wake of one
/// waiter per killed owner after its physical exit; L2 pins their order.
/// Before that, the waiters stayed parked in the precise futex model and the
/// run ended in a deadlock report.
#[test]
fn ptrace_wakes_the_waiter_when_a_sigkill_ends_the_owner() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-sigkill-ptrace-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);
    for (mode, owners) in SIGKILL_MODES {
        let run = run_lifecycle_mode("ptrace", &build_root, &guest, mode, false);
        assert!(
            run.stdout.contains("PASS: waiter received EOWNERDEAD")
                && (owners == 1
                    || run
                        .stdout
                        .contains("PASS: second waiter received EOWNERDEAD")),
            "ptrace {mode}: the waiter did not get EOWNERDEAD\nstdout:\n{}\nstderr:\n{}",
            run.stdout,
            run.stderr,
        );
        let wakes: Vec<&str> = run
            .stderr
            .lines()
            .filter(|line| line.contains("robust-list owner death woke"))
            .collect();
        assert!(
            wakes.len() == owners
                && wakes.iter().all(|wake| {
                    wake.contains("woke 1 waiter(s)") && wake.contains("after physical exit")
                }),
            "ptrace {mode}: expected {owners} modeled wake(s) of one waiter each after the owners' exits, got {wakes:#?}\nstderr:\n{}",
            run.stderr,
        );
        let verified = run_lifecycle_mode("ptrace", &build_root, &guest, mode, true);
        assert!(
            verified.stdout.contains("PASS: waiter received EOWNERDEAD"),
            "ptrace {mode} verification did not make progress after owner death\nstdout:\n{}\nstderr:\n{}",
            verified.stdout,
            verified.stderr,
        );
    }
}

/// A successful execve ends robust-mutex owners too. Past exec's point of no
/// return, `de_thread` kills every other thread, whose exits run
/// `futex_exit_release`, and `exec_mm_release` runs `futex_exec_release` for
/// the caller (kernel/fork.c, kernel/futex/core.c); each walks its robust list.
/// Detcore reads the group's lists at the exec's entry and wakes one modeled
/// waiter per owner word once the exec has returned
/// (https://github.com/rrnewton/hermit/issues/2082). The three modes cover a
/// non-leader exec that kills the owning leader (`de-thread`), the owner's own
/// exec (`owner-exec`), a leader exec that kills an owning sibling
/// (`leader-exec`), and a non-leader caller whose own list holds a word owned
/// by the thread group ID, which its walk under that ID matches
/// (`thread-tgid-exec`). Before the fix each left the waiter parked for good.
#[test]
fn ptrace_wakes_the_waiter_when_an_exec_ends_the_owner() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-lifecycle-ptrace-exec-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);

    for mode in ["de-thread", "owner-exec", "leader-exec", "thread-tgid-exec"] {
        let verify_json = build_root.join(format!("ptrace-{mode}.verify.json"));
        let _ = fs::remove_file(&verify_json);
        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "120s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=info",
                "--backend=ptrace",
                "run",
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
                "--log=info",
                "--backend=ptrace",
                "run",
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
        let exec_wakes: Vec<&str> = captured
            .stderr
            .lines()
            .filter(|line| line.contains("robust-list owner death woke"))
            .collect();
        assert!(
            exec_wakes.len() == 1
                && exec_wakes[0].contains("woke 1 waiter(s) on futex")
                && exec_wakes[0].ends_with("at exec"),
            "ptrace {mode} did not wake exactly one modeled waiter, once, at the exec\nstderr:\n{}",
            captured.stderr,
        );
    }
}

/// The exec neighbours in which Linux wakes nobody, each through a raw
/// `FUTEX_WAIT` with a 3-second timeout so the guest reports what happened
/// instead of blocking in glibc:
///
/// - `thread-owner-exec`: a thread that is not the leader owns the mutex and
///   execs. `de_thread` gives it the leader's TID (`exchange_tids`) before
///   `futex_exec_release` walks its list, so `handle_futex_death` sees a word
///   owned under the old TID and neither marks nor wakes it: the waiter times
///   out (https://github.com/rrnewton/hermit/issues/2082).
/// - `failed-exec`: the owner's exec fails before the point of no return, so
///   nothing happens until the owner unlocks.
///
/// Detcore must make no modeled owner-death wake in either case.
#[test]
fn ptrace_wakes_nobody_when_an_exec_leaves_the_owner_word_alone() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-lifecycle-ptrace-exec-nowake-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);

    for (mode, required) in [
        (
            "thread-owner-exec",
            "RAW: wait ETIMEDOUT, owner unlocked 0, OWNER_DIED 0",
        ),
        (
            "failed-exec",
            "RAW: wait woken, owner unlocked 1, OWNER_DIED 0",
        ),
    ] {
        let mut native = Command::new("timeout");
        native
            .args(["--kill-after", "5s", "30s"])
            .arg(&guest)
            .arg(mode);
        let control = run_captured(
            native,
            &format!("native robust-futex {mode} control"),
            &build_root.join(format!("native-{mode}.out")),
            &build_root.join(format!("native-{mode}.err")),
            true,
        );
        // Natively a failed exec's waiter can also find the word already
        // unlocked (EAGAIN); either way it never returns before the unlock.
        let native_holds = if mode == "failed-exec" {
            control.stdout.contains("owner unlocked 1, OWNER_DIED 0")
        } else {
            control.stdout.contains(required)
        };
        assert!(
            native_holds,
            "native {mode} control did not behave as Linux is expected to\nstdout:\n{}\nstderr:\n{}",
            control.stdout, control.stderr,
        );

        let verify_json = build_root.join(format!("ptrace-{mode}.verify.json"));
        let _ = fs::remove_file(&verify_json);
        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "120s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=info",
                "--backend=ptrace",
                "run",
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
            verified.stdout.contains(required),
            "ptrace {mode} verification did not match Linux\nstdout:\n{}\nstderr:\n{}",
            verified.stdout,
            verified.stderr,
        );
        assert_nonempty_canonical_l2(&verify_json, &verified.stdout, &verified.stderr);

        let mut run = Command::new("timeout");
        run.args(["--kill-after", "5s", "120s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=info",
                "--backend=ptrace",
                "run",
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
            captured.stdout.contains(required),
            "ptrace {mode} did not match Linux\nstdout:\n{}\nstderr:\n{}",
            captured.stdout,
            captured.stderr,
        );
        assert!(
            !captured.stderr.contains("robust-list owner death woke"),
            "ptrace {mode} made an owner-death wake that Linux does not make\nstderr:\n{}",
            captured.stderr,
        );
    }
}

/// Linux's exec walk marks each owned word once and wakes one waiter on it;
/// the second waiter on the word times out. With two raw waiters on one word,
/// a second modeled wake would wake both (Claude review of
/// https://github.com/rrnewton/hermit/pull/4352, F1: a mutant delivering every
/// wake twice passed the single-waiter tests).
fn assert_an_exec_wakes_one_waiter_per_word(backend: &str) {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("robust-futex-exec-two-waiters-{backend}"));
    let guest = build_lifecycle_guest(&build_root);
    let expected = "EXEC-TWO-WAITERS: woken=1 timed_out=1 word=0xc0000000";
    let mut native = Command::new(&guest);
    native.arg("exec-two-waiters");
    let native = run_captured(
        native,
        "native robust-futex exec-two-waiters",
        &build_root.join("native.out"),
        &build_root.join("native.err"),
        true,
    );
    assert!(
        native.stdout.contains(expected),
        "native: {}",
        native.stdout
    );
    for verify in [true, false] {
        let run = run_lifecycle_mode(backend, &build_root, &guest, "exec-two-waiters", verify);
        assert!(
            run.stdout.contains(expected),
            "{backend} exec-two-waiters (verify {verify}) differs from Linux\nstdout:\n{}\nstderr:\n{}",
            run.stdout,
            run.stderr,
        );
    }
}

#[test]
fn ptrace_an_exec_wakes_one_waiter_per_word_it_marks() {
    assert_an_exec_wakes_one_waiter_per_word("ptrace");
}

/// Runs the lifecycle guest in `mode` under `backend` with `--strict`, at L2
/// when `verify` is set, and requires that no killed thread outwaited an
/// exit_group walk.
fn run_lifecycle_mode(
    backend: &str,
    build_root: &Path,
    guest: &Path,
    mode: &str,
    verify: bool,
) -> Captured {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "120s"])
        .arg(hermit_test::hermit_binary())
        .arg("--log=info")
        .arg(format!("--backend={backend}"))
        .args(["run", "--strict", "--tmp=/tmp"]);
    let verify_json = build_root.join(format!("{backend}-{mode}.verify.json"));
    if verify {
        let _ = fs::remove_file(&verify_json);
        command
            .args(["--verify", "--verify-strict", "--verify-json"])
            .arg(&verify_json);
    }
    command
        .args(["--base-env=minimal", "--"])
        .arg(guest)
        .arg(mode);
    let kind = if verify { "verify" } else { "run" };
    let captured = run_captured(
        command,
        &format!("{backend} robust-futex {mode} {kind}"),
        &build_root.join(format!("{backend}-{mode}-{kind}.out")),
        &build_root.join(format!("{backend}-{mode}-{kind}.err")),
        true,
    );
    if verify {
        assert_nonempty_canonical_l2(&verify_json, &captured.stdout, &captured.stderr);
    }
    // A killed thread's wait for the exit_group walk must end with the walk,
    // never at its bound (which fails the run with this name).
    assert!(
        !captured.stderr.contains("EXIT_GROUP_ROBUST_WALK_TIMEOUT"),
        "a killed thread outwaited the exit_group walk\nstderr:\n{}",
        captured.stderr,
    );
    captured
}

/// The ptrace leg of `dbt_wakes_a_shared_alias_once_and_each_pending_slot_once_as_linux_does`.
/// ptrace stages a group's wakes from a read-only walk too, so a word reached
/// twice (two mappings of one page, the second entry looping on itself) must
/// read as marked the second time, as Linux's own store makes it: without that,
/// ptrace woke both shared-alias waiters where Linux wakes one, and no ptrace
/// test noticed (Claude review of 65138a17, N2).
#[test]
fn ptrace_wakes_a_shared_alias_once_and_each_pending_slot_once_as_linux_does() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-boundary-ptrace-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);
    for (mode, expected) in [
        (
            "shared-alias",
            "BOUNDARY: mode=shared-alias woken=1 timed_out=1 word=0xc0000000",
        ),
        (
            "two-pending",
            "BOUNDARY: mode=two-pending woken=2 timed_out=0 word=0",
        ),
        (
            "pending-marked",
            "BOUNDARY: mode=pending-marked woken=2 timed_out=0 word=0xc0000000",
        ),
        (
            "pending-marked-quiet",
            "BOUNDARY: mode=pending-marked-quiet woken=2 timed_out=0 word=0x40000000",
        ),
    ] {
        let mut native = Command::new(&guest);
        native.arg(mode);
        let native = run_captured(
            native,
            &format!("native robust-futex {mode}"),
            &build_root.join(format!("native-{mode}.out")),
            &build_root.join(format!("native-{mode}.err")),
            true,
        );
        assert!(
            native.stdout.contains(expected),
            "native {mode}: {}",
            native.stdout
        );
        let run = run_lifecycle_mode("ptrace", &build_root, &guest, mode, false);
        assert!(
            run.stdout.contains(expected),
            "ptrace {mode} differs from Linux\nstdout:\n{}\nstderr:\n{}",
            run.stdout,
            run.stderr,
        );
    }
}

/// A robust entry that is its own successor reaches one word twice in one
/// exit. Linux marks it once and wakes one waiter; Detcore's ptrace walk read
/// it as still owned the second time and woke a second waiter (Codex review of
/// https://github.com/rrnewton/hermit/pull/4277 at 65138a17). Recorded and
/// replayed on ptrace at L2, the one wake matches Linux, and RECORD_VERSION
/// refuses a recording made before the change, whose replay would succeed
/// with one waiter fewer timed out.
#[test]
fn ptrace_records_a_self_aliased_word_woken_once_and_refuses_older_recordings() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-self-alias-record-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);
    let expected = "BOUNDARY: mode=self-alias woken=1 timed_out=1 word=0xc0000000";
    let mut native = Command::new(&guest);
    native.arg("self-alias");
    let native = run_captured(
        native,
        "native robust-futex self-alias",
        &build_root.join("native-self-alias.out"),
        &build_root.join("native-self-alias.err"),
        true,
    );
    assert!(
        native.stdout.contains(expected),
        "native: {}",
        native.stdout
    );

    let verify_json = build_root.join("record.verify.json");
    let _ = fs::remove_file(&verify_json);
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "5s", "120s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "--backend=ptrace",
            "record",
            "start",
            "--strict",
            "--record-timeout=60",
            "--verify",
            "--verify-strict",
            "--verify-json",
        ])
        .arg(&verify_json)
        .arg("--")
        .arg(&guest)
        .arg("self-alias");
    let verified = run_captured(
        record,
        "ptrace robust-futex self-alias record and replay",
        &build_root.join("record-verify.out"),
        &build_root.join("record-verify.err"),
        true,
    );
    assert!(
        verified.stdout.contains(expected),
        "the recorded run differs from Linux\nstdout:\n{}\nstderr:\n{}",
        verified.stdout,
        verified.stderr,
    );
    assert_nonempty_canonical_l2(&verify_json, &verified.stdout, &verified.stderr);

    // A kept recording, replayed as made and then relabelled 0x14f.
    let data_dir = build_root.join("recording");
    let _ = fs::remove_dir_all(&data_dir);
    fs::create_dir_all(&data_dir).expect("failed to create the recording directory");
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "5s", "120s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=off",
            "--backend=ptrace",
            "record",
            "start",
            "--strict",
            "--record-timeout=60",
        ])
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("--")
        .arg(&guest)
        .arg("self-alias");
    let recorded = run_captured(
        record,
        "ptrace robust-futex self-alias record",
        &build_root.join("record.out"),
        &build_root.join("record.err"),
        true,
    );
    assert!(
        recorded.stdout.contains(expected),
        "recorded: {}",
        recorded.stdout
    );
    let replay = |label: &str, name: &str, expect_success: bool| {
        let mut replay = Command::new("timeout");
        replay
            .args(["--kill-after", "5s", "120s"])
            .arg(hermit_test::hermit_binary())
            .args(["--log=off", "replay", "--autopilot"])
            .arg(format!("--data-dir={}", data_dir.display()));
        run_captured(
            replay,
            label,
            &build_root.join(format!("{name}.out")),
            &build_root.join(format!("{name}.err")),
            expect_success,
        )
    };
    // The untouched recording replays, so the refusal below is the gate's.
    let accepted = replay("replay of the current recording", "replay-current", true);
    assert!(
        accepted.stdout.contains(expected),
        "replayed: {}",
        accepted.stdout
    );

    let metadata_path = fs::read_dir(&data_dir)
        .expect("failed to read the recording directory")
        .map(|entry| {
            entry
                .expect("recording directory entry")
                .path()
                .join("metadata.json")
        })
        .find(|candidate| candidate.is_file())
        .expect("the recording has a metadata.json");
    let mut metadata: serde_json::Value = serde_json::from_slice(
        &fs::read(&metadata_path).expect("failed to read the recording metadata"),
    )
    .expect("recording metadata is not JSON");
    // 0x150 made this change; later changes moved the version on (0x154 is
    // https://github.com/rrnewton/hermit/pull/4314, 0x155 the x86-64-v3
    // profile of https://github.com/rrnewton/hermit/issues/4243, 0x156
    // https://github.com/rrnewton/hermit/issues/4266's B3a-1, 0x157
    // https://github.com/rrnewton/hermit/pull/4351, 0x158 B3a-2 of
    // https://github.com/rrnewton/hermit/issues/4266, 0x159 the process cgroup
    // of https://github.com/rrnewton/hermit/issues/4378, 0x15a /proc/loadavg
    // rendered by Detcore for https://github.com/rrnewton/hermit/issues/4213, 0x15b
    // https://github.com/rrnewton/hermit/issues/4343, 0x15c
    // https://github.com/rrnewton/hermit/issues/4410, 0x15d /proc/uptime
    // rendered by Detcore for https://github.com/rrnewton/hermit/issues/4213,
    // 0x15e https://github.com/rrnewton/hermit/issues/4370, 0x15f and 0x160
    // R2b-1 of https://github.com/rrnewton/hermit/issues/4273 and its revert,
    // 0x161 https://github.com/rrnewton/hermit/issues/4430, 0x162
    // https://github.com/rrnewton/hermit/issues/4440, 0x163 /proc/stat
    // rendered by Detcore for https://github.com/rrnewton/hermit/issues/4213,
    // 0x164 the R2b-1 reland of https://github.com/rrnewton/hermit/issues/4273,
    // 0x165 sendfile recording its copy for
    // https://github.com/rrnewton/hermit/issues/4416, 0x166
    // https://github.com/rrnewton/hermit/issues/4362 part 2 step 0b, 0x167 an
    // open through a descriptor link records its target's path, 0x168 POSIX
    // timer SI_TIMER records for https://github.com/rrnewton/hermit/issues/4262,
    // 0x169 /proc/cpuinfo rendered by Detcore for
    // https://github.com/rrnewton/hermit/issues/4213, 0x16a a relative name
    // below a received directory descriptor is the kernel's recorded lookup,
    // 0x16c the K3a revert's relabel of 0x16a's format after 0x16b shipped,
    // 0x16d the container mount table rendered by Detcore for
    // https://github.com/rrnewton/hermit/issues/4213, 0x16e a restartable call
    // blocks signals before its recorded attempt for R2b-2 stage 2 of
    // https://github.com/rrnewton/hermit/issues/4273, 0x16f a recorded fstat
    // after each OFD lock, for the guest's own /proc/locks rows, 0x171 the
    // R2b-2 stage 3 revert's relabel of 0x16f's format after 0x170 shipped,
    // 0x172 the descriptor-table ledger's /proc/locks decision and its
    // deferred fdinfo refusal,
    // 0x173 a recorded private SIGCHLD ends a gated wait for R2b-2 stage 3 of
    // https://github.com/rrnewton/hermit/issues/4273, re-landed,
    // 0x176 the fdinfo read through an anchor proved to be procfs,
    // 0x177 truthful timer capability and physical-arm separation, reserved
    // as 0x175 and taken at land as HIGHEST_SHIPPED + 1,
    // 0x178 the procfs anchor's entry rechecked as procfs, and
    // 0x179 a vfork parent resumed at its child's exec or exit,
    // 0x17a first-instruction DBT lifecycle admission,
    // 0x17b the vfork rejoin reverted,
    // 0x17c a readmitted thread keeps its call's mask until its continuation,
    // 0x17d the vfork rejoin re-landed,
    // 0x17e and 0x17f calls whose mask probe a signal stopped restart,
    // 0x180 sigreturn continuations under record and replay for
    // https://github.com/rrnewton/hermit/issues/4505, and
    // 0x181 the /proc/locks capture drops only repeated rows, and
    // 0x182 private DBT runtime images stay out of application mapping space,
    // 0x183 authenticates bootstrap LOAD ownership in the guest maps view,
    // 0x184 projects those private intervals from complete Smaps sections,
    // 0x185 random devices are classified by device number,
    // 0x186 projects coherent NUMA rows with authenticated private ownership,
    // 0x187 a recording stores the schedule it ran under,
    // 0x188 runs a MSG_DONTWAIT call on a socketpair end once,
    // 0x189 replays a recorded poll's ERESTARTSYS as ERESTARTNOHAND,
    // 0x18a gives each open of a random device a stream of its own,
    // 0x18b keeps ignored signals from ending kernel-run epoll waits,
    // 0x18c and kernel-run poll and select waits that name no mask,
    // 0x18f a descriptor received over SCM_RIGHTS joins the open file its
    // send modelled,
    // 0x190 a fatal signal retires the process's other threads in its turn,
    // 0x193 every /proc status file shows TracerPid 0,
    // and 0x195 runs a kernel-run poll's restart as the guest's own
    // restart_syscall),
    // and a recording from before the change, 0x14f, is still refused by
    // the exact-match gate.
    assert_eq!(
        metadata["version"], 0x19a,
        "RECORD_VERSION moved; point this test at the current version and the one before"
    );
    metadata["version"] = serde_json::json!(0x14f);
    fs::write(
        &metadata_path,
        serde_json::to_vec(&metadata).expect("failed to serialize the metadata"),
    )
    .expect("failed to rewrite the recording metadata");
    let refused = replay(
        "replay of the recording relabelled 0x14f",
        "replay-0x14f",
        false,
    );
    assert!(
        refused.stderr.contains("Version mismatch"),
        "a 0x14f recording was not refused by the version gate\nstdout:\n{}\nstderr:\n{}",
        refused.stdout,
        refused.stderr,
    );
    assert!(
        !refused.stdout.contains("BOUNDARY:"),
        "the 0x14f recording's guest ran before the refusal\nstdout:\n{}",
        refused.stdout,
    );
}
