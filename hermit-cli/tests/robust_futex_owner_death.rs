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

/// Every line of a Hermit log that records a robust-list owner-death store or
/// wake, in order.
#[cfg(feature = "dbt")]
fn owner_death_records(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|line| line.contains("robust-list owner death"))
        .collect()
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

#[cfg(feature = "dbt")]
fn build_fatal_signal_guest(build_root: &Path) -> PathBuf {
    fs::create_dir_all(build_root).expect("failed to create fatal-signal guest build directory");
    let guest = build_root.join("fatal_signal_threads");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(repository().join("tests/c/fatal_signal_threads.c"))
        .args(["-pthread", "-o"])
        .arg(&guest);
    run_captured(
        compile,
        "fatal-signal guest compilation",
        &build_root.join("compile-fatal-signal.out"),
        &build_root.join("compile-fatal-signal.err"),
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

/// The DBT leg of `ptrace_wakes_the_waiter_when_an_exec_ends_the_owner`. DBT
/// runs the native execve outside Detcore's handler, and the new image starts a
/// fresh runtime, so Detcore reads the group's lists before the exec and hands
/// the wakes to the coordinator, which delivers them when the new image
/// reconnects (https://github.com/rrnewton/hermit/issues/4268). Before, each of
/// these modes left the waiter parked until the run timed out.
/// `thread-tgid-exec` needs DBT's client to rewrite a non-leader caller's word
/// naming the virtual thread group ID to the host one before the native exec,
/// so that Linux's walk under that ID marks it
/// (https://github.com/rrnewton/reverie/issues/1052, fixed by
/// https://github.com/rrnewton/reverie/pull/1053).
#[cfg(feature = "dbt")]
#[test]
fn dbt_wakes_the_waiter_when_an_exec_ends_the_owner() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-lifecycle-dbt-exec-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);
    for mode in ["de-thread", "owner-exec", "leader-exec", "thread-tgid-exec"] {
        for verify in [true, false] {
            let run = run_dbt_lifecycle_mode(&build_root, &guest, mode, verify);
            assert!(
                run.stdout.contains("PASS: waiter received EOWNERDEAD"),
                "DBT {mode} (verify {verify}) did not wake the waiter\nstdout:\n{}\nstderr:\n{}",
                run.stdout,
                run.stderr,
            );
        }
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

#[cfg(feature = "dbt")]
#[test]
fn dbt_an_exec_wakes_one_waiter_per_word_it_marks() {
    assert_an_exec_wakes_one_waiter_per_word("dbt");
}

#[test]
fn ptrace_an_exec_wakes_one_waiter_per_word_it_marks() {
    assert_an_exec_wakes_one_waiter_per_word("ptrace");
}

/// The DBT leg of `ptrace_wakes_nobody_when_an_exec_leaves_the_owner_word_alone`:
/// an exec whose walk marks nothing (`thread-owner-exec`) and an exec that
/// fails (`failed-exec`) wake nobody, as on Linux, at L2.
#[cfg(feature = "dbt")]
#[test]
fn dbt_wakes_nobody_when_an_exec_leaves_the_owner_word_alone() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-lifecycle-dbt-exec-nowake-{}",
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
        let run = run_dbt_lifecycle_mode(&build_root, &guest, mode, true);
        assert!(
            run.stdout.contains(required),
            "DBT {mode} did not match Linux\nstdout:\n{}\nstderr:\n{}",
            run.stdout,
            run.stderr,
        );
    }
}

/// DBT reports a thread's exit to Detcore before the native exit runs. Detcore
/// walks the list, leaves each owner word to Linux and wakes the modeled
/// waiter, and the scheduler commits no turn until the kernel has ended the
/// thread, so the waiter runs only after Linux has marked the word
/// (https://github.com/rrnewton/hermit/issues/4238). Before that, DBT did not
/// model owner death at all, and this guest ended in Detcore's "Deadlock
/// detected" report with the waiter parked.
#[cfg(feature = "dbt")]
#[test]
fn dbt_robust_futex_owner_death_wakes_the_waiter_at_l2() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-owner-death-dbt-{}",
        std::process::id()
    ));
    let guest = build_guest(&build_root);
    let verify_json = build_root.join("dbt.verify.json");
    let _ = fs::remove_file(&verify_json);

    let mut verify = Command::new("timeout");
    verify
        .args(["--kill-after", "5s", "120s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "--backend=dbt",
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
        "DBT robust-futex owner-death strict verification",
        &build_root.join("verify.out"),
        &build_root.join("verify.err"),
        true,
    );
    assert!(
        captured
            .stdout
            .contains("PASS: robust mutex waiter received EOWNERDEAD"),
        "the waiter did not observe EOWNERDEAD under Hermit DBT\nstdout:\n{}\nstderr:\n{}",
        captured.stdout,
        captured.stderr,
    );
    assert_nonempty_canonical_l2(&verify_json, &captured.stdout, &captured.stderr);
}

/// The DBT leg of `detcore_wakes_the_modeled_waiter_before_linux_finishes_exit`:
/// as on ptrace, Detcore leaves the owner word to Linux's own exit and wakes
/// exactly one modeled waiter; it makes no store itself. The record of the
/// word is at DEBUG because it carries a raw guest address.
#[cfg(feature = "dbt")]
#[test]
fn dbt_detcore_leaves_the_owner_word_to_linux_and_wakes_the_waiter() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("robust-futex-dbt-evidence-{}", std::process::id()));
    let guest = build_guest(&build_root);

    let mut run = Command::new("timeout");
    run.args(["--kill-after", "5s", "120s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=debug",
            "--backend=dbt",
            "run",
            "--strict",
            "--tmp=/tmp",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest);
    let captured = run_captured(
        run,
        "DBT robust-futex owner-death instrumented run",
        &build_root.join("run.out"),
        &build_root.join("run.err"),
        true,
    );
    let stdout = captured.stdout;
    let stderr = captured.stderr;
    assert!(
        stdout.contains("PASS: robust mutex waiter received EOWNERDEAD"),
        "the waiter did not observe EOWNERDEAD under Hermit DBT\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // Every owner-death record, in order: the word left to Linux, then one
    // wake that reached exactly one modeled waiter.
    let records = owner_death_records(&stderr);
    assert_eq!(
        records.len(),
        2,
        "owner-death records: {records:#?}\nstderr:\n{stderr}"
    );
    assert!(
        records[0].contains("leaving futex word")
            && records[0].contains("for backend exit cleanup"),
        "Detcore did not leave the owner word to Linux: {records:#?}"
    );
    assert!(
        records[1].contains("robust-list owner death woke 1 waiter(s)"),
        "Detcore's owner-death wake did not reach exactly one modeled waiter: {records:#?}"
    );
    assert!(
        !stderr.contains("robust-list owner death: stored futex word"),
        "Detcore stored the owner word itself\nstderr:\n{stderr}"
    );
}

/// An exit_group by a thread that does not own the robust mutex ends the
/// owning sibling too, and Linux walks that sibling's list as it dies. On DBT,
/// Detcore walks the group's lists and wakes the waiter in the exit_group's
/// own handler, after its grant has logically killed the group, and no turn
/// runs until the kernel has ended every thread of the group
/// (https://github.com/rrnewton/hermit/issues/4238). The killed sibling holds
/// its native exit until that walk is done: otherwise Linux marks the word
/// (clearing the owner's TID) before Detcore reads it, and in about a third
/// of runs the waiter stayed parked.
#[cfg(feature = "dbt")]
#[test]
fn dbt_wakes_the_waiter_when_an_exit_group_ends_the_owner() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("robust-futex-lifecycle-dbt-{}", std::process::id()));
    let guest = build_lifecycle_guest(&build_root);
    let verify_json = build_root.join("dbt-exit-group.verify.json");
    let _ = fs::remove_file(&verify_json);

    let mut verify = Command::new("timeout");
    verify
        .args(["--kill-after", "5s", "120s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "--backend=dbt",
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
        .arg("exit-group");
    let captured = run_captured(
        verify,
        "DBT robust-futex exit-group strict verification",
        &build_root.join("dbt-exit-group-verify.out"),
        &build_root.join("dbt-exit-group-verify.err"),
        true,
    );
    assert!(
        captured.stdout.contains("PASS: waiter received EOWNERDEAD"),
        "the waiter did not observe EOWNERDEAD after exit_group under Hermit DBT\nstdout:\n{}\nstderr:\n{}",
        captured.stdout,
        captured.stderr,
    );
    assert_nonempty_canonical_l2(&verify_json, &captured.stdout, &captured.stderr);

    let mut run = Command::new("timeout");
    run.args(["--kill-after", "5s", "120s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "--backend=dbt",
            "run",
            "--strict",
            "--tmp=/tmp",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .arg("exit-group");
    let captured = run_captured(
        run,
        "DBT robust-futex exit-group run",
        &build_root.join("dbt-exit-group.out"),
        &build_root.join("dbt-exit-group.err"),
        true,
    );
    assert!(
        captured.stdout.contains("PASS: waiter received EOWNERDEAD"),
        "the waiter did not observe EOWNERDEAD after exit_group under Hermit DBT\nstdout:\n{}\nstderr:\n{}",
        captured.stdout,
        captured.stderr,
    );
    // Exactly one owner-death wake record, reaching one modeled waiter. The
    // store record is DEBUG, and absent when Linux marked the word first.
    let wakes: Vec<&str> = owner_death_records(&captured.stderr)
        .into_iter()
        .filter(|record| record.contains("robust-list owner death woke"))
        .collect();
    assert_eq!(
        wakes.len(),
        1,
        "Detcore must issue exactly one owner-death wake: {wakes:#?}\nstderr:\n{}",
        captured.stderr,
    );
    assert!(
        wakes[0].contains("woke 1 waiter(s)"),
        "the owner-death wake did not reach exactly one modeled waiter: {wakes:#?}"
    );
    assert!(
        !captured.stderr.contains("after physical exit"),
        "DBT took the staged path that waits for exit callbacks\nstderr:\n{}",
        captured.stderr,
    );
}

/// The DBT leg of the `signal` mode of
/// `ptrace_wakes_after_guest_observed_fatal_signal_and_exit_group_cleanup`: a
/// thread of the owner's process raises SIGTERM, whose default action ends the
/// process, and the waiter is woken with EOWNERDEAD, at L2 and in a plain run.
/// Before, DynamoRIO's process exit waited forever to synchronize with the
/// owner thread, which was blocked waiting for its turn, and the run was
/// killed at its deadline (https://github.com/rrnewton/hermit/issues/4278).
#[cfg(feature = "dbt")]
#[test]
fn dbt_wakes_the_waiter_when_a_fatal_signal_ends_the_owner() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-lifecycle-dbt-signal-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);
    for verify in [true, false] {
        let run = run_dbt_lifecycle_mode(&build_root, &guest, "signal", verify);
        assert!(
            run.stdout.contains("PASS: waiter received EOWNERDEAD"),
            "DBT signal (verify {verify}) did not wake the waiter\nstdout:\n{}\nstderr:\n{}",
            run.stdout,
            run.stderr,
        );
        if !verify {
            // Exactly one owner-death wake, reaching one modeled waiter, as
            // for an exit_group (Claude review of
            // https://github.com/rrnewton/hermit/pull/4387, finding 3).
            let wakes = owner_death_wakes(&run.stderr);
            assert_eq!(
                wakes.len(),
                1,
                "Detcore must issue exactly one owner-death wake: {wakes:#?}\nstderr:\n{}",
                run.stderr,
            );
            assert!(
                wakes[0].contains("woke 1 waiter(s)"),
                "the owner-death wake did not reach exactly one modeled waiter: {wakes:#?}"
            );
        }
    }
}

/// The same fatal SIGTERM with no robust mutex at all: a forked child with a
/// second thread blocked in pause() is ended by the signal's default action,
/// and its parent sees it killed by SIGTERM, at L2 and in a plain run. The
/// child with one thread, and a child with a SIGTERM handler, whose handler
/// must still run, are the controls. Each output must match the native run
/// (https://github.com/rrnewton/hermit/issues/4278).
///
/// In `threaded-race` the sibling's next turn falls right after the
/// delivery's decision. It would install a SIGTERM handler and then store to
/// a page the parent prints. Linux kills every other thread when it commits
/// the default action, so "signaled sig=15 shm=1" never happens there. Hermit
/// grants the group exit before any other thread's turn, so the sibling never
/// runs after the decision: "shm=0", and the action never needs restoring
/// (Codex review and Claude re-check of
/// https://github.com/rrnewton/hermit/pull/4387).
#[cfg(feature = "dbt")]
#[test]
fn dbt_a_default_fatal_signal_ends_a_threaded_process_and_a_handler_still_runs() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("fatal-signal-threads-dbt-{}", std::process::id()));
    let guest = build_fatal_signal_guest(&build_root);
    for (mode, expected) in [
        (
            "single-default",
            "FATAL-SIGNAL single-default: signaled sig=15\n",
        ),
        (
            "threaded-default",
            "FATAL-SIGNAL threaded-default: signaled sig=15\n",
        ),
        (
            "threaded-caught",
            "FATAL-SIGNAL threaded-caught: exited code=0\n",
        ),
    ] {
        let mut native = Command::new("timeout");
        native
            .args(["--kill-after", "5s", "30s"])
            .arg(&guest)
            .arg(mode);
        let control = run_captured(
            native,
            &format!("native fatal-signal {mode} control"),
            &build_root.join(format!("native-{mode}.out")),
            &build_root.join(format!("native-{mode}.err")),
            true,
        );
        assert_eq!(control.stdout, expected, "native {mode} control");
        for verify in [true, false] {
            let run = run_dbt_lifecycle_mode(&build_root, &guest, mode, verify);
            assert_eq!(
                run.stdout, expected,
                "DBT {mode} (verify {verify})\nstderr:\n{}",
                run.stderr
            );
        }
    }

    let mut native = Command::new("timeout");
    native
        .args(["--kill-after", "5s", "30s"])
        .arg(&guest)
        .arg("threaded-race");
    let control = run_captured(
        native,
        "native fatal-signal threaded-race control",
        &build_root.join("native-threaded-race.out"),
        &build_root.join("native-threaded-race.err"),
        true,
    );
    assert!(
        [
            "FATAL-SIGNAL threaded-race: signaled sig=15 shm=0\n",
            "FATAL-SIGNAL threaded-race: exited code=0 shm=1\n",
            "FATAL-SIGNAL threaded-race: exited code=0 shm=0\n",
        ]
        .contains(&control.stdout.as_str()),
        "native threaded-race control: {}",
        control.stdout
    );
    for verify in [true, false] {
        let run = run_dbt_lifecycle_mode(&build_root, &guest, "threaded-race", verify);
        assert_eq!(
            run.stdout, "FATAL-SIGNAL threaded-race: signaled sig=15 shm=0\n",
            "DBT threaded-race (verify {verify})\nstderr:\n{}",
            run.stderr
        );
        if !verify {
            assert_eq!(
                run.stderr
                    .matches("ends its thread group by default")
                    .count(),
                1,
                "the delivery did not decide the default action\nstderr:\n{}",
                run.stderr
            );
            assert_eq!(
                run.stderr
                    .matches("changed after the delivery decision")
                    .count(),
                0,
                "a sibling ran between the decision and the group exit\nstderr:\n{}",
                run.stderr
            );
        }
    }
}

/// Runs the lifecycle guest's `mode` under Hermit DBT with `--strict` and
/// returns the capture: with `--verify --verify-strict` (and the report's L2
/// checked) when `verify`, else at `--log=info`.
#[cfg(feature = "dbt")]
fn run_dbt_lifecycle_mode(build_root: &Path, guest: &Path, mode: &str, verify: bool) -> Captured {
    run_lifecycle_mode("dbt", build_root, guest, mode, verify)
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

/// The owner-death wake records of a `--log=info` run.
#[cfg(feature = "dbt")]
fn owner_death_wakes(stderr: &str) -> Vec<&str> {
    owner_death_records(stderr)
        .into_iter()
        .filter(|record| record.contains("robust-list owner death woke"))
        .collect()
}

/// A sibling blocked in pthread_mutex_lock on the owner's mutex keeps that
/// mutex in its pending slot. Created first, it is walked first. The caller
/// holds 1,500 robust mutexes, whose walk gave the owner's native exit time to
/// mark the word first (Claude re-check of
/// https://github.com/rrnewton/hermit/pull/4277: the wake was then recorded
/// under the sibling in 20 of 20 runs). The killed threads now hold their
/// native exits until the walk is done, so the one wake is recorded under the
/// owner in every run, and the strict comparison of two runs' INFO records
/// matches.
#[cfg(feature = "dbt")]
#[test]
fn dbt_wakes_a_blocked_siblings_mutex_once_under_its_owner() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-blocked-sibling-dbt-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);
    let verified = run_dbt_lifecycle_mode(&build_root, &guest, "blocked-sibling", true);
    assert!(
        verified.stdout.contains("PASS: waiter received EOWNERDEAD"),
        "stdout:\n{}\nstderr:\n{}",
        verified.stdout,
        verified.stderr,
    );
    let run = run_dbt_lifecycle_mode(&build_root, &guest, "blocked-sibling", false);
    let owner = run
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("OWNER: tid "))
        .unwrap_or_else(|| panic!("the guest did not report its owner\n{}", run.stdout));
    let wakes = owner_death_wakes(&run.stderr);
    assert_eq!(
        wakes.len(),
        1,
        "owner-death wakes: {wakes:#?}\nstderr:\n{}",
        run.stderr
    );
    let recorded = format!("[detcore, dtid {owner}] robust-list owner death woke 1 waiter(s)");
    assert!(
        wakes[0].contains(&recorded),
        "the wake must be recorded under the owner, dtid {owner}: {wakes:#?}"
    );
}

/// A thread that registers its robust list only after another thread has
/// filed its exit_group, before the grant: Linux walks the registration
/// current when the thread dies, so the waiter is woken and the word marked.
/// Under Hermit the registration lands in that window, so an exit_group walk
/// that read the registrations when it was filed would miss it (measured: the
/// waiter timed out with the word registered, 3 of 3 runs).
#[cfg(feature = "dbt")]
#[test]
fn dbt_walks_a_registration_made_after_the_exit_group_is_filed() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-late-registration-dbt-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);
    let run = run_dbt_lifecycle_mode(&build_root, &guest, "late-registration", false);
    assert!(
        run.stdout
            .contains("RAW: wait woken, owner unlocked 0, OWNER_DIED 1, registered 1"),
        "stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr,
    );
    let wakes = owner_death_wakes(&run.stderr);
    assert_eq!(
        wakes.len(),
        1,
        "owner-death wakes: {wakes:#?}\nstderr:\n{}",
        run.stderr
    );
    assert!(wakes[0].contains("woke 1 waiter(s)"), "{wakes:#?}");
}

/// Cases from the Codex and Claude re-checks of
/// https://github.com/rrnewton/hermit/pull/4277, natively and under Hermit
/// DBT, which must agree: one word reached through two mappings of a shared
/// page is woken once (one of two waiters times out); and two threads pending
/// on one ownerless word each wake once, whether the word is free or already
/// marked by an earlier owner's death, with or without waiters (both waiters
/// are woken, the word is unchanged).
#[cfg(feature = "dbt")]
#[test]
fn dbt_wakes_a_shared_alias_once_and_each_pending_slot_once_as_linux_does() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("robust-futex-boundary-dbt-{}", std::process::id()));
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
        let run = run_dbt_lifecycle_mode(&build_root, &guest, mode, false);
        assert!(
            run.stdout.contains(expected),
            "DBT {mode} differs from Linux\nstdout:\n{}\nstderr:\n{}",
            run.stdout,
            run.stderr,
        );
    }
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
    // 0x182 private DBT runtime images stay out of application mapping space),
    // and a recording from before the change, 0x14f, is still refused by
    // the exact-match gate.
    assert_eq!(
        metadata["version"], 0x182,
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

/// The owned word sits only in its owner's pending slot (glibc's state inside
/// pthread_mutex_unlock), and a sibling walked first waits on it with it in
/// its own pending slot (Claude re-check of c65d9202, NF2). The threads the
/// exit_group kills hold their native exits until its walk is done, so the
/// owner's native exit cannot mark the word before the sibling's slot is read:
/// the one wake is recorded under the owner in every run, and two runs' INFO
/// records match.
#[cfg(feature = "dbt")]
#[test]
fn dbt_wakes_an_owners_pending_word_under_its_owner() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-pending-owner-dbt-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);
    let verified = run_dbt_lifecycle_mode(&build_root, &guest, "pending-owner", true);
    assert!(
        verified
            .stdout
            .contains("PENDING-OWNER: waiter woken word=0xc0000000"),
        "stdout:\n{}\nstderr:\n{}",
        verified.stdout,
        verified.stderr,
    );
    let run = run_dbt_lifecycle_mode(&build_root, &guest, "pending-owner", false);
    let owner = run
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("OWNER: tid "))
        .unwrap_or_else(|| panic!("the guest did not report its owner\n{}", run.stdout));
    let wakes = owner_death_wakes(&run.stderr);
    assert_eq!(
        wakes.len(),
        1,
        "owner-death wakes: {wakes:#?}\nstderr:\n{}",
        run.stderr
    );
    let recorded = format!("[detcore, dtid {owner}] robust-list owner death woke 1 waiter(s)");
    assert!(
        wakes[0].contains(&recorded),
        "the wake must be recorded under the owner, dtid {owner}: {wakes:#?}"
    );
}

/// Two threads of one process call exit_group at once, as two threads calling
/// exit() do; each owns a process-shared robust mutex with a waiter in
/// another process (Claude re-check of 3719cd4e, F1). Whichever exit_group is
/// granted, the other filer is killed by it and must hold its native exit until
/// the winner's walk is done: both waiters get EOWNERDEAD, as on Linux, the
/// two wakes are recorded under A and then B, and two runs match. A thread
/// created first holds 2,000 robust mutexes, so the walk reaches the killed
/// filer's list late (Claude review of 65138a17, N1): with a killed filer
/// exempt from the wait, a waiter timed out in 5 of 5 runs.
#[cfg(feature = "dbt")]
#[test]
fn dbt_wakes_both_owners_waiters_when_two_threads_call_exit_group() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-concurrent-exit-group-dbt-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);
    let verified = run_dbt_lifecycle_mode(&build_root, &guest, "concurrent-exit-group", true);
    assert!(
        verified
            .stdout
            .contains("CONCURRENT-EXIT-GROUP: A=EOWNERDEAD B=EOWNERDEAD"),
        "stdout:\n{}\nstderr:\n{}",
        verified.stdout,
        verified.stderr,
    );
    let run = run_dbt_lifecycle_mode(&build_root, &guest, "concurrent-exit-group", false);
    assert!(
        run.stdout
            .contains("CONCURRENT-EXIT-GROUP: A=EOWNERDEAD B=EOWNERDEAD"),
        "stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr,
    );
    let tid = |name: &str| {
        run.stdout
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap_or_else(|| panic!("the guest did not report {name}\n{}", run.stdout))
            .to_owned()
    };
    let (a, b) = (tid("A: tid "), tid("B: tid "));
    let wakes = owner_death_wakes(&run.stderr)
        .into_iter()
        .filter(|wake| !wake.contains("woke 0 waiter(s)"))
        .collect::<Vec<_>>();
    assert_eq!(
        wakes.len(),
        2,
        "owner-death wakes: {wakes:#?}\nstderr:\n{}",
        run.stderr
    );
    assert!(
        wakes[0].contains(&format!(
            "[detcore, dtid {a}] robust-list owner death woke 1 waiter(s)"
        )) && wakes[1].contains(&format!(
            "[detcore, dtid {b}] robust-list owner death woke 1 waiter(s)"
        )),
        "the wakes must come from A's walk, then B's (A={a}, B={b}): {wakes:#?}"
    );
}

/// A lock owned by thread B linked into thread A's list, with B pending on it
/// (Codex re-check of 5899d5c8, P2-3,
/// https://github.com/rrnewton/hermit/issues/4302). A's walk, first in owner
/// order, skips that word and wakes A's own; B's walk then wakes the word from
/// its pending slot. With the killed threads holding their native exits, the
/// two wakes are recorded under A and then B in every run.
#[cfg(feature = "dbt")]
#[test]
fn dbt_wakes_a_reversed_owner_registration_as_linux_does() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-futex-reversed-owner-dbt-{}",
        std::process::id()
    ));
    let guest = build_lifecycle_guest(&build_root);
    let verified = run_dbt_lifecycle_mode(&build_root, &guest, "reversed-owner", true);
    assert!(
        verified
            .stdout
            .contains("REVERSED-OWNER: W=woken U=woken words=0xc0000000,0xc0000000"),
        "stdout:\n{}\nstderr:\n{}",
        verified.stdout,
        verified.stderr,
    );
    let run = run_dbt_lifecycle_mode(&build_root, &guest, "reversed-owner", false);
    let tid = |name: &str| {
        run.stdout
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap_or_else(|| panic!("the guest did not report {name}\n{}", run.stdout))
            .to_owned()
    };
    let (a, b) = (tid("A: tid "), tid("B: tid "));
    let wakes = owner_death_wakes(&run.stderr);
    assert_eq!(
        wakes.len(),
        2,
        "owner-death wakes: {wakes:#?}\nstderr:\n{}",
        run.stderr
    );
    assert!(
        wakes[0].contains(&format!(
            "[detcore, dtid {a}] robust-list owner death woke 1 waiter(s)"
        )) && wakes[1].contains(&format!(
            "[detcore, dtid {b}] robust-list owner death woke 1 waiter(s)"
        )),
        "the wakes must come from A's walk, then B's (A={a}, B={b}): {wakes:#?}"
    );
}
