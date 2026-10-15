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
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use hermit::HERMIT_INTERNAL_FAILURE_EXIT;
use hermit::HERMIT_VERIFICATION_DIVERGENCE_EXIT;

const DETERMINISM_RUNS: usize = 5;
const DEADLOCK_RUNS: usize = 3;
const DEADLOCK_BOUND: Duration = Duration::from_secs(5);
const DEADLOCK_CLEANUP_BOUND: Duration = Duration::from_secs(1);
const DEADLOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

static HERMIT_SIGNAL_LOCK: Mutex<()> = Mutex::new(());
static SIGNAL_GUEST: OnceLock<PathBuf> = OnceLock::new();

fn command_output(mut command: Command, label: &str) -> Output {
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn kill_created_process_group(pid: u32, label: &str) {
    let result = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            panic!("failed to kill created {label} process group {pid}: {error}");
        }
    }
}

fn reap_killed_child(child: &mut std::process::Child, label: &str) -> std::process::ExitStatus {
    let deadline = Instant::now() + DEADLOCK_CLEANUP_BOUND;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) if Instant::now() >= deadline => {
                panic!(
                    "created {label} process group did not exit within {DEADLOCK_CLEANUP_BOUND:?} after SIGKILL"
                );
            }
            Ok(None) => std::thread::sleep(DEADLOCK_POLL_INTERVAL),
            Err(error) => panic!("failed to reap killed {label}: {error}"),
        }
    }
}

fn bounded_command_output(mut command: Command, label: &str) -> (Output, bool, Duration) {
    hermit_test::configure_guest_execution(&mut command);
    // A typed PMU skid-overshoot refusal, and nothing else, runs it again.
    let (output, (timed_out, elapsed)) = hermit_test::hermit_output_with(label, || {
        let (output, timed_out, elapsed) = bounded_command_output_once(&mut command, label);
        (output, (timed_out, elapsed))
    });
    (output, timed_out, elapsed)
}

/// One attempt of [`bounded_command_output`].
fn bounded_command_output_once(command: &mut Command, label: &str) -> (Output, bool, Duration) {
    let mut stdout = tempfile::tempfile()
        .unwrap_or_else(|error| panic!("failed to create {label} stdout capture: {error}"));
    let mut stderr = tempfile::tempfile()
        .unwrap_or_else(|error| panic!("failed to create {label} stderr capture: {error}"));
    command
        .process_group(0)
        .stdout(Stdio::from(stdout.try_clone().unwrap_or_else(|error| {
            panic!("failed to clone {label} stdout capture: {error}")
        })))
        .stderr(Stdio::from(stderr.try_clone().unwrap_or_else(|error| {
            panic!("failed to clone {label} stderr capture: {error}")
        })));
    let rendered = format!("{command:?}");
    let started = Instant::now();
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    let (status, process_timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) if started.elapsed() >= DEADLOCK_BOUND => {
                kill_created_process_group(child.id(), label);
                break (reap_killed_child(&mut child, label), true);
            }
            Ok(None) => std::thread::sleep(DEADLOCK_POLL_INTERVAL),
            Err(error) => {
                kill_created_process_group(child.id(), label);
                let _ = reap_killed_child(&mut child, label);
                panic!("failed to poll {label}: {rendered}: {error}");
            }
        }
    };
    stdout
        .seek(SeekFrom::Start(0))
        .unwrap_or_else(|error| panic!("failed to rewind {label} stdout capture: {error}"));
    stderr
        .seek(SeekFrom::Start(0))
        .unwrap_or_else(|error| panic!("failed to rewind {label} stderr capture: {error}"));
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout
        .read_to_end(&mut stdout_bytes)
        .unwrap_or_else(|error| panic!("failed to read {label} stdout capture: {error}"));
    stderr
        .read_to_end(&mut stderr_bytes)
        .unwrap_or_else(|error| panic!("failed to read {label} stderr capture: {error}"));
    let elapsed = started.elapsed();
    (
        Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        },
        process_timed_out || elapsed >= DEADLOCK_BOUND,
        elapsed,
    )
}

fn hermit_signal_lock() -> MutexGuard<'static, ()> {
    HERMIT_SIGNAL_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn signal_guest() -> &'static Path {
    SIGNAL_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("signal-determinism-{}", std::process::id()));
            fs::create_dir_all(&build_root)
                .expect("failed to create signal determinism build directory");
            let binary = build_root.join("signal_determinism");

            let mut command = Command::new("cc");
            command
                .args([
                    "-O0",
                    "-g",
                    "-pthread",
                    "-D_GNU_SOURCE",
                    "-std=c11",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/signal_determinism.c"))
                .arg("-o")
                .arg(&binary);
            command_output(command, "signal guest compilation");
            binary
        })
        .as_path()
}

fn run_signal_scenario_on_backend(
    backend: Option<&str>,
    scenario: &str,
    expected_stdout: &str,
    runs: usize,
) {
    let _guard = hermit_signal_lock();
    let mut baseline = None;

    for iteration in 0..runs {
        let mut command = Command::new(hermit_test::hermit_binary());
        if let Some(backend) = backend {
            command.arg(format!("--backend={backend}"));
        }
        command.arg("run");
        command.args([
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--",
        ]);
        command.arg(signal_guest()).arg(scenario);
        let output = command_output(
            command,
            &format!("signal scenario {scenario}, iteration {}", iteration + 1),
        );
        assert_eq!(
            output.stdout,
            expected_stdout.as_bytes(),
            "unexpected output for signal scenario {scenario}, iteration {}\nstderr:\n{}",
            iteration + 1,
            String::from_utf8_lossy(&output.stderr),
        );

        if let Some(first) = &baseline {
            assert_eq!(
                &output.stdout,
                first,
                "signal scenario {scenario} changed output on iteration {}",
                iteration + 1,
            );
        } else {
            baseline = Some(output.stdout);
        }
    }
}

fn run_signal_scenario(scenario: &str, expected_stdout: &str) {
    run_signal_scenario_on_backend(None, scenario, expected_stdout, DETERMINISM_RUNS);
}

#[test]
fn sigalrm_itimer_delivery_is_deterministic() {
    run_signal_scenario(
        "itimer-delivery",
        "alarm delivered\nalarm pending=1 phase=2 deliveries=1\n",
    );
}

#[test]
fn armed_itimer_is_discarded_on_process_exit() {
    run_signal_scenario("itimer-exit", "timer discarded after process exit\n");
}

#[test]
fn signal_interrupts_emulated_blocking_read() {
    run_signal_scenario(
        "blocking-read-interrupted",
        "blocking read interrupted deliveries=1 bytes=xx\n",
    );
}

#[test]
fn signal_restarts_emulated_blocking_read() {
    run_signal_scenario(
        "blocking-read-restarted",
        "blocking read restarted deliveries=1 bytes=xx\n",
    );
}

#[test]
fn signal_interrupts_poll_despite_sa_restart() {
    run_signal_scenario("poll-sa-restart", "poll interrupted deliveries=1\n");
}

#[test]
fn signal_interrupts_epoll_wait_despite_sa_restart() {
    run_signal_scenario(
        "epoll-wait-sa-restart",
        "epoll_wait interrupted deliveries=1\n",
    );
}

#[test]
fn signal_interrupts_rt_sigtimedwait_despite_sa_restart() {
    run_signal_scenario(
        "sigtimedwait-sa-restart",
        "rt_sigtimedwait interrupted deliveries=1 pending=SIGUSR2\n",
    );
}

#[test]
fn sigsuspend_invalid_arguments_preserve_linux_error_order() {
    run_signal_scenario(
        "sigsuspend-invalid-arguments",
        "rt_sigsuspend invalid size=EINVAL invalid pointer=EFAULT\n",
    );
}

#[cfg(feature = "dbt")]
#[test]
fn dbt_sigsuspend_invalid_pointer_does_not_fault_in_the_tool() {
    run_signal_scenario_on_backend(
        Some("dbt"),
        "sigsuspend-invalid-arguments",
        "rt_sigsuspend invalid size=EINVAL invalid pointer=EFAULT\n",
        1,
    );
}

/// On DBT, a guest's SIGKILL of another process ends every thread of that
/// process, as Linux does: the parent's waitpid reports the child killed by
/// SIGKILL, with one thread and with a second thread paused, whether the kill
/// is sent with kill(2) or with tgkill(2) to the child's main thread. Before,
/// a kill(2) to a two-threaded process was refused (Detcore does not choose a
/// process signal's recipient thread), the kill notice retired only the
/// thread it named, and a tgkill(2) SIGKILL retired nothing
/// (https://github.com/rrnewton/hermit/issues/4362).
///
/// The sender's kill also returns only once the victim is gone, as ptrace's
/// SIGKILL fence makes it: the parent's SIGCHLD handler has run and the pipe
/// the child held is at EOF before the parent's next instruction, so each
/// count is 0 in every run. Before the fence these counts varied from run to
/// run (Claude review of https://github.com/rrnewton/hermit/pull/4400, F1).
/// The same holds for a pidfd SIGKILL to a process whose leader thread has
/// exited, which stopped with a false deadlock and then a named refusal: the
/// kill notice now finds the victim through a surviving thread. A pidfd
/// SIGKILL to a zombie leaves the parent its child's own exit status
/// (https://github.com/rrnewton/hermit/issues/4411).
/// These are plain runs: `--verify` of a run whose child is SIGKILLed fails
/// DBT's evidence check, which requires a FINAL frame that a killed process
/// cannot write (https://github.com/rrnewton/hermit/issues/4396).
#[cfg(feature = "dbt")]
#[test]
fn dbt_sigkill_ends_every_thread_of_the_target() {
    for (scenario, expected) in SIGKILL_SCENARIOS {
        run_signal_scenario_on_backend(Some("dbt"), scenario, expected, DETERMINISM_RUNS);
    }
}

/// On DBT, a tgkill SIGKILL that names a child's leader after that thread has
/// exited, while a sibling runs on, does not end the group on Linux: the
/// child is still alive. Hermit cannot find the victim's host process from
/// that thread, and a thread-directed kill gets no fallback through a
/// sibling, so the run is refused by name before the sender is answered,
/// never ended as if the group had been killed (Claude review of
/// https://github.com/rrnewton/hermit/pull/4439, R1). The same holds for a
/// pidfd SIGKILL with PIDFD_SIGNAL_THREAD, which also names one thread
/// (Claude re-check of 4439, N1), and for tkill and rt_tgsigqueueinfo (N4).
#[cfg(feature = "dbt")]
#[test]
fn dbt_refuses_a_thread_directed_sigkill_to_an_exited_thread() {
    let _guard = hermit_signal_lock();
    for (mode, seed) in [
        ("sigkill-tgkill-exited-leader", None),
        ("sigkill-tgkill-exited-leader", Some(1)),
        ("sigkill-tgkill-exited-leader", Some(2)),
        ("sigkill-tgkill-exited-leader", Some(3)),
        ("sigkill-pidfd-thread-exited-leader", None),
        ("sigkill-pidfd-thread-exited-leader", Some(1)),
        ("sigkill-tkill-exited-leader", None),
        ("sigkill-tkill-exited-leader", Some(1)),
        ("sigkill-tgsigqueue-exited-leader", None),
        ("sigkill-tgsigqueue-exited-leader", Some(1)),
    ] {
        let mut command = Command::new(hermit_test::hermit_binary());
        command.args([
            "--backend=dbt",
            "run",
            "--strict",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
        ]);
        match seed {
            Some(seed) => command.args(["--chaos", &format!("--sched-seed={seed}")]),
            None => command.arg("--max-timeslice=disabled"),
        };
        command.arg("--");
        command.arg(signal_guest()).arg(mode);
        let label = format!("DBT {mode}, chaos seed {seed:?}");
        let (output, timed_out, elapsed) = bounded_command_output(command, &label);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !timed_out,
            "{label} hit the host bound after {elapsed:?}\n{stderr}"
        );
        assert_eq!(
            output.status.code(),
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            "{label} was not refused\nstdout:\n{}\nstderr:\n{stderr}",
            String::from_utf8_lossy(&output.stdout),
        );
        for expected in [
            "cannot be held until it is gone",
            "a SIGKILL to that thread alone does not end its group",
        ] {
            assert!(
                stderr.contains(expected),
                "{label} missing {expected:?}:\n{stderr}"
            );
        }
        assert!(
            !stderr.contains("HERMIT_DEADLOCK"),
            "{label} reached the kill notice's false deadlock:\n{stderr}"
        );
    }
}

/// The ptrace control for `dbt_sigkill_ends_every_thread_of_the_target`.
#[test]
fn ptrace_sigkill_ends_every_thread_of_the_target() {
    for (scenario, expected) in SIGKILL_SCENARIOS {
        run_signal_scenario_on_backend(Some("ptrace"), scenario, expected, 1);
    }
}

const SIGKILL_SCENARIOS: [(&str, &str); 10] = [
    (
        "sigkill-child",
        "sigkill child threaded=0 signaled=1 sig=9\n",
    ),
    (
        "sigkill-threaded-child",
        "sigkill child threaded=1 signaled=1 sig=9\n",
    ),
    (
        "sigkill-tgkill-child",
        "sigkill by tgkill child threaded=0 signaled=1 sig=9\n",
    ),
    (
        "sigkill-tgkill-threaded-child",
        "sigkill by tgkill child threaded=1 signaled=1 sig=9\n",
    ),
    (
        "sigkill-sigchld",
        "sigkill sigchld threaded=0 iterations=0 signaled=1 sig=9\n",
    ),
    (
        "sigkill-threaded-sigchld",
        "sigkill sigchld threaded=1 iterations=0 signaled=1 sig=9\n",
    ),
    (
        "sigkill-pipe-eof",
        "sigkill pipe-eof threaded=0 iterations=0 signaled=1 sig=9\n",
    ),
    (
        "sigkill-threaded-pipe-eof",
        "sigkill pipe-eof threaded=1 iterations=0 signaled=1 sig=9\n",
    ),
    (
        "sigkill-pidfd-leader-exited-sigchld",
        "sigkill sigchld by pidfd after leader exit threaded=1 iterations=0 signaled=1 sig=9\n",
    ),
    (
        "sigkill-pidfd-zombie",
        "sigkill pidfd zombie send=0 exited=1 code=7\n",
    ),
];

#[test]
fn blocking_sigsuspend_releases_the_scheduler() {
    run_signal_scenario(
        "blocking-sigsuspend",
        "sigsuspend delivered\nsigsuspend restored=1 deliveries=1\n",
    );
}

#[test]
fn pending_signal_completes_sigsuspend_and_restores_mask() {
    run_signal_scenario(
        "pending-sigsuspend",
        "sigsuspend delivered\npending sigsuspend restored=1 deliveries=1 pending=0\n",
    );
}

#[test]
fn sigsuspend_without_signal_reports_terminal_deadlock() {
    let _guard = hermit_signal_lock();
    let mut baseline_stderr = None;

    for iteration in 0..DEADLOCK_RUNS {
        let mut command = Command::new(hermit_test::hermit_binary());
        command.args([
            "--log=off",
            "run",
            // Compare complete diagnostics from the same clock input, including
            // committed virtual time; do not strip or quantize their contents.
            "--epoch=2026-01-01T00:00:00.123456789Z",
            "--strict",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--base-env=minimal",
            "--",
        ]);
        command
            .arg(signal_guest())
            .arg("blocking-sigsuspend-no-signal");
        let label = format!("no-signal sigsuspend scenario, iteration {}", iteration + 1);
        let (output, timed_out, elapsed) = bounded_command_output(command, &label);
        assert!(
            !timed_out,
            "{label} exceeded the {DEADLOCK_BOUND:?} host bound; the scheduler emitted no terminal verdict"
        );
        assert_eq!(
            output.status.code(),
            Some(HERMIT_INTERNAL_FAILURE_EXIT),
            "{label} did not exit with the scheduler deadlock status in {elapsed:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            output.stdout, b"sigsuspend waiting without a signal\n",
            "{label} did not reach rt_sigsuspend"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        for expected in [
            "Deadlock detected: thread(s) waiting in rt_sigsuspend with no possible signal, but no runnable threads left.",
            "external IO blockers: none",
            "rt_sigsuspend blockers (1), by dettid:",
        ] {
            assert!(
                stderr.contains(expected),
                "{label} missing {expected:?}:\n{stderr}"
            );
        }
        assert!(
            !stderr.contains("unexpected sigsuspend return"),
            "{label} returned from sigsuspend instead of diagnosing the wait:\n{stderr}"
        );

        if let Some(first) = &baseline_stderr {
            assert_eq!(
                output.stderr,
                *first,
                "no-signal sigsuspend diagnostic changed on iteration {}",
                iteration + 1
            );
        } else {
            baseline_stderr = Some(output.stderr);
        }
    }
}

#[test]
fn signal_masks_survive_fork_and_clone() {
    run_signal_scenario(
        "masks-fork-clone",
        "parent mask=blocked\nfork mask=blocked\nclone mask=blocked\n",
    );
}

#[test]
fn signal_handler_reentrance_is_deterministic() {
    run_signal_scenario(
        "handler-reentrance",
        "handler depth=1\nhandler depth=2\nreentrant deliveries=2 max_depth=2\n",
    );
}

#[test]
fn alternate_signal_stack_is_preserved() {
    run_signal_scenario(
        "altstack-preservation",
        "altstack handler\naltstack handler\naltstack deliveries=2 preserved=1\n",
    );
}

#[test]
fn pending_signal_and_mask_survive_exec() {
    run_signal_scenario(
        "pending-exec",
        "exec mask=blocked pending=preserved consumed=SIGUSR1\n",
    );
}

/// Runs `script` under `hermit run --verify` with `options`, which must exit
/// with `status`, and returns its stderr, which must report `run` terminated by
/// SIGUSR1 and end that report with the guest's last system call, its `kill`.
fn verify_signal_report(options: &[&str], script: &str, run: &str, status: i32) -> String {
    let mut command = Command::new(hermit_test::hermit_binary());
    command
        .args(["run", "--verify", "--base-env=minimal"])
        .args(options)
        .args(["--", "/bin/sh", "-c", script]);
    hermit_test::configure_guest_execution(&mut command);
    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(status),
        "unexpected status\nstderr:\n{stderr}"
    );
    let heading = format!(":: The {run} guest was terminated by signal 10 (SIGUSR1).");
    let report = stderr
        .split_once(&heading)
        .unwrap_or_else(|| panic!("no {heading:?} report\nstderr:\n{stderr}"))
        .1;
    let records: Vec<&str> = report
        .lines()
        .skip(1)
        .take_while(|line| line.starts_with("::   "))
        .collect();
    assert!(
        records.iter().all(|record| record.contains(" INFO ")
            && record.contains(" DETLOG ")
            && !record.contains("DETLOG_RECORD")),
        "the report holds a record that is not a bare INFO DETLOG line\nstderr:\n{stderr}"
    );
    assert!(
        records
            .last()
            .is_some_and(|record| record.contains("kill(")),
        "the report does not end at the guest's kill\nstderr:\n{stderr}"
    );
    stderr
}

#[test]
fn verify_reports_the_last_records_of_a_run_1_killed_by_a_signal() {
    let _guard = hermit_signal_lock();
    let stderr = verify_signal_report(
        &[],
        "echo before; kill -USR1 $$",
        "run 1",
        HERMIT_INTERNAL_FAILURE_EXIT,
    );
    assert!(
        stderr.contains("First run errored during --verify"),
        "{stderr}"
    );
}

/// A marker file named `name` that does not exist yet, which a guest creates
/// in run 1 so that run 2 can behave differently.
fn run_1_marker(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("verify-signal-report-{}", std::process::id()));
    fs::create_dir_all(&directory).expect("failed to create the marker directory");
    let marker = directory.join(name);
    match fs::remove_file(&marker) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", marker.display()),
    }
    marker
}

#[test]
fn verify_reports_the_last_records_of_a_run_2_killed_by_a_signal() {
    let _guard = hermit_signal_lock();
    let marker = run_1_marker("run-1-finished");
    // Run 1 leaves the marker and succeeds; run 2 finds it and is killed.
    let script = format!(
        "if [ -e '{0}' ]; then kill -USR1 $$; else : > '{0}'; fi",
        marker.display()
    );
    // The comparison then reports the runs as nondeterministic.
    let stderr = verify_signal_report(&[], &script, "run 2", 1);
    assert!(!stderr.contains("The run 1 guest"), "{stderr}");
}

/// `--verify-allow=both` admits run 2's signal, and the comparison still
/// rejects the runs, whose statuses differ: the signal is reported.
#[test]
fn verify_reports_an_admitted_run_2_signal_the_comparison_rejects() {
    let _guard = hermit_signal_lock();
    let marker = run_1_marker("run-1-finished-allow-both");
    let script = format!(
        "if [ -e '{0}' ]; then kill -USR1 $$; else : > '{0}'; fi",
        marker.display()
    );
    let stderr = verify_signal_report(&["--verify-allow=both"], &script, "run 2", 1);
    assert!(!stderr.contains("The run 1 guest"), "{stderr}");
}

/// `--verify-allow=failure` admits run 1's signal, so run 2 runs, exits 1, and
/// the comparison rejects the runs: run 1's signal is reported from the log
/// the comparison consumed.
#[test]
fn verify_reports_an_admitted_run_1_signal_the_comparison_rejects() {
    let _guard = hermit_signal_lock();
    let marker = run_1_marker("run-1-killed-allow-failure");
    let script = format!(
        "if [ -e '{0}' ]; then exit 1; else : > '{0}'; kill -USR1 $$; fi",
        marker.display()
    );
    let stderr = verify_signal_report(&["--verify-allow=failure"], &script, "run 1", 1);
    assert!(
        !stderr.contains("First run errored during --verify"),
        "{stderr}"
    );
    assert!(!stderr.contains("The run 2 guest"), "{stderr}");
}

/// Two runs killed by the same signal at the same point verify under
/// `--verify-allow=failure`; the report is only for a rejected verification.
#[test]
fn verify_writes_no_signal_report_when_the_signaled_runs_verify() {
    let _guard = hermit_signal_lock();
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args([
        "run",
        "--verify",
        "--base-env=minimal",
        "--verify-allow=failure",
        "--",
        "/bin/sh",
        "-c",
        "echo before; kill -USR1 $$",
    ]);
    hermit_test::configure_guest_execution(&mut command);
    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // hermit ends as the verified guest did, by the same signal.
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGUSR1),
        "unexpected status\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Determinism verified."),
        "the runs did not verify\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("guest was terminated by signal"),
        "a verified run wrote a signal report\nstderr:\n{stderr}"
    );
}

/// The signal report is a diagnostic: it must not replace or delay the
/// verification's own disposition when stderr cannot take it.
///
/// Under `--max-log-bytes` stderr here is a 4096-byte pipe in `O_NONBLOCK`
/// mode that nobody reads until hermit exits, and the guest's last records
/// name long paths, so the report is larger than the pipe. Written with
/// `eprint!`, the report fails with `EAGAIN` once the pipe is full and the
/// print macro panics: hermit exits 101 instead of the rejected first run's
/// 125.
#[test]
fn verify_signal_report_cannot_replace_the_disposition_on_a_full_stderr() {
    assert_signal_report_cannot_hold_the_disposition(&["--max-log-bytes=64M"], true, 4096, 0);
}

/// The same without `--max-log-bytes`, on a blocking pipe: the report must
/// not wait for room itself, and must not fill the pipe ahead of the
/// disposition hermit still has to write (`main`'s error report waits for a
/// reader to make room).
///
/// The pipe has two pages, and 2048 bytes already wait in it, so the report
/// cannot fit in what is left: written with an ordinary write, it waits.
/// Linux adds a short write only to a pipe's last page, and the report is
/// longer than what is left of the first, so it ends on a full second page
/// and an ordinary write behind it waits too. With one page the report could
/// only top up the first, leaving the room a disposition written after it
/// needs, and the order would go untested.
#[test]
fn verify_signal_report_cannot_hold_the_disposition_on_a_full_blocking_stderr() {
    assert_signal_report_cannot_hold_the_disposition(&[], false, 8192, 2048);
}

/// The guest output both runs agreed on is required output too, and the
/// report must not hold it either. Both runs print `x` with no newline, so
/// hermit's stdout still buffers it when the report is written, and are
/// killed by SIGUSR1; `--verify-allow=failure` admits that. Only the result
/// of run 1's check for the marker differs, which keeps the comparison's
/// report of the difference short (about 4.7 KB), and the runs are rejected
/// (status 1) with both reports kept (about 13 KB).
///
/// stdout and stderr are one blocking pipe of four pages with 5632 bytes
/// waiting. What hermit must write ends in the third page (anywhere from
/// about 2.5 KB to 6.5 KB of it does), leaving the fourth empty; run 1's
/// report is larger than what is left, so it fills the fourth page, and
/// Linux adds a short write only to a last page with room. Flushed only at
/// exit, the `x` waits behind it until the bound kills hermit; in any other
/// geometry it would arrive after the report, which fails too.
#[test]
fn verify_signal_report_cannot_hold_agreed_guest_stdout() {
    let _guard = hermit_signal_lock();
    let marker = run_1_marker("run-1-marked-agreed-stdout");
    let script = format!(
        "[ -e '{0}' ]; : > '{0}'; {1}; printf x; kill -USR1 $$",
        marker.display(),
        twenty_long_path_checks(),
    );
    let pipe = UndrainedPipe {
        bytes: 16384,
        waiting: 5632,
        nonblocking: false,
        stdout_too: true,
    };
    let Some((status, delivered)) = run_against_an_undrained_pipe(
        &[
            "run",
            "--verify",
            "--verify-allow=failure",
            "--base-env=minimal",
        ],
        &script,
        pipe,
    ) else {
        return;
    };
    assert_eq!(
        status.code(),
        Some(HERMIT_VERIFICATION_DIVERGENCE_EXIT),
        "the report replaced the rejected runs' status\ndelivered:\n{delivered}"
    );
    let heading = ":: The run 1 guest was terminated by signal 10 (SIGUSR1).";
    let (before, _) = delivered.split_once(heading).unwrap_or_else(|| {
        panic!("the report's first line fits in the pipe and must arrive\ndelivered:\n{delivered}")
    });
    assert!(
        before.ends_with('x'),
        "the agreed guest stdout must arrive, ahead of the report\ndelivered:\n{delivered}"
    );
}

/// The control for the two tests below: with room in the stdout file, the
/// agreed `x` that both verified runs print with no newline reaches it, and
/// hermit then ends by the guest's signal. Before hermit flushed stdout itself,
/// raising the signal discarded the `x`.
#[test]
fn verify_delivers_agreed_unterminated_stdout_before_the_guest_signal() {
    let (status, appended, stderr) = run_with_stdout_held_in_a_file(
        &["--verify-allow=failure"],
        "printf x; kill -USR1 $$",
        StdoutFile {
            size_limited: false,
        },
    );
    assert_eq!(
        status.signal(),
        Some(libc::SIGUSR1),
        "unexpected status {status:?}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Determinism verified."),
        "the runs did not verify\nstderr:\n{stderr}"
    );
    assert_eq!(
        appended, b"x",
        "the agreed guest stdout must arrive\nstderr:\n{stderr}"
    );
}

/// The same with the stdout file already at the file-size limit and SIGXFSZ
/// at its default: writing the `x` fails, and hermit must still end by the
/// guest's SIGUSR1 rather than be killed by SIGXFSZ for its own write.
#[test]
fn verify_keeps_the_guest_signal_when_stdout_is_at_its_size_limit() {
    let (status, appended, stderr) = run_with_stdout_held_in_a_file(
        &["--verify-allow=failure"],
        "printf x; kill -USR1 $$",
        StdoutFile { size_limited: true },
    );
    assert_eq!(
        status.signal(),
        Some(libc::SIGUSR1),
        "hermit's own write replaced the guest's signal: {status:?}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Determinism verified."),
        "the runs did not verify\nstderr:\n{stderr}"
    );
    assert!(
        appended.is_empty(),
        "the file grew past its limit: {appended:?}"
    );
}

/// And for a status hermit exits with: the runs differ only in run 1's check
/// for the marker and are rejected (1). `exit` flushes stdout again, so the
/// `x` that could not be written is written again there, and that write must
/// not be killed by SIGXFSZ either.
#[test]
fn verify_keeps_the_rejected_status_when_stdout_is_at_its_size_limit() {
    let marker = run_1_marker("run-1-marked-stdout-at-limit");
    let script = format!(
        "[ -e '{0}' ]; : > '{0}'; printf x; kill -USR1 $$",
        marker.display()
    );
    let (status, appended, stderr) = run_with_stdout_held_in_a_file(
        &["--verify-allow=failure"],
        &script,
        StdoutFile { size_limited: true },
    );
    assert_eq!(
        status.code(),
        Some(HERMIT_VERIFICATION_DIVERGENCE_EXIT),
        "hermit's own write replaced the rejected runs' status: {status:?}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains(":: The run 1 guest was terminated by signal 10 (SIGUSR1)."),
        "the runs were not rejected for their difference\nstderr:\n{stderr}"
    );
    assert!(
        appended.is_empty(),
        "the file grew past its limit: {appended:?}"
    );
}

/// hermit's stdout: a file opened for appending that already holds
/// `STDOUT_FILE_HELD` bytes (sparse), so hermit appends at that offset.
struct StdoutFile {
    /// Whether hermit runs with `RLIMIT_FSIZE` at `STDOUT_FILE_HELD`, so the
    /// file is already at the limit. The limit is high enough for every file
    /// hermit writes from the start. SIGXFSZ is at its default and unblocked
    /// either way.
    size_limited: bool,
}

const STDOUT_FILE_HELD: u64 = 1 << 30;

/// Run `hermit run --verify --base-env=minimal` with `extra_args` and then
/// `-- /bin/sh -c script`, its stdout `file`, and return its status, within a
/// bound, what it appended to the file, and its stderr.
fn run_with_stdout_held_in_a_file(
    extra_args: &[&str],
    script: &str,
    file: StdoutFile,
) -> (std::process::ExitStatus, Vec<u8>, String) {
    let _guard = hermit_signal_lock();
    let mut stdout = tempfile::tempfile().expect("failed to create the stdout file");
    stdout
        .set_len(STDOUT_FILE_HELD)
        .expect("failed to size the stdout file");
    let appending = fs::OpenOptions::new()
        .append(true)
        .open(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(&stdout)
        ))
        .expect("failed to open the stdout file for appending");
    let mut stderr = tempfile::tempfile().expect("failed to create the stderr file");

    let mut command = Command::new(hermit_test::hermit_binary());
    command
        .args(["run", "--verify", "--base-env=minimal"])
        .args(extra_args)
        .args(["--", "/bin/sh", "-c", script]);
    // Before stdio, as in `run_against_an_undrained_pipe`.
    hermit_test::configure_guest_execution(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(appending)
        .stderr(stderr.try_clone().expect("failed to clone the stderr file"));
    let size_limited = file.size_limited;
    // SAFETY: only async-signal-safe calls, on local values, in the child.
    unsafe {
        command.pre_exec(move || {
            if size_limited {
                let limit = libc::rlimit {
                    rlim_cur: STDOUT_FILE_HELD,
                    rlim_max: STDOUT_FILE_HELD,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            libc::signal(libc::SIGXFSZ, libc::SIG_DFL);
            let mut xfsz: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut xfsz);
            libc::sigaddset(&mut xfsz, libc::SIGXFSZ);
            libc::sigprocmask(libc::SIG_UNBLOCK, &xfsz, std::ptr::null_mut());
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start hermit: {error}"));
    drop(command);
    let deadline = Duration::from_secs(30);
    let status = wait_within(&mut child, deadline);

    let mut appended = Vec::new();
    stdout
        .seek(SeekFrom::Start(STDOUT_FILE_HELD))
        .expect("failed to seek the stdout file");
    stdout
        .read_to_end(&mut appended)
        .expect("failed to read the stdout file");
    let mut stderr_bytes = Vec::new();
    stderr
        .seek(SeekFrom::Start(0))
        .expect("failed to seek the stderr file");
    stderr
        .read_to_end(&mut stderr_bytes)
        .expect("failed to read the stderr file");
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    let status = status
        .unwrap_or_else(|| panic!("hermit had not exited after {deadline:?}\nstderr:\n{stderr}"));
    (status, appended, stderr)
}

/// Wait for `child` until `deadline` has passed since now; then kill and reap
/// it and return `None`.
fn wait_within(
    child: &mut std::process::Child,
    deadline: Duration,
) -> Option<std::process::ExitStatus> {
    let started = Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None if started.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Run a first run that a signal ends and `--verify` rejects, with stderr a
/// `pipe_bytes` pipe holding `waiting_bytes` that nobody reads until hermit
/// exits, `O_NONBLOCK` when `nonblocking`, and require the rejected first
/// run's status, within a bound, with the disposition and the report's first
/// line delivered. The report (20 records naming a 300-byte path, about
/// 7.8 KB) is larger than a page.
fn assert_signal_report_cannot_hold_the_disposition(
    global_args: &[&str],
    nonblocking: bool,
    pipe_bytes: i32,
    waiting_bytes: usize,
) {
    let _guard = hermit_signal_lock();
    let script = format!("{}; kill -USR1 $$", twenty_long_path_checks());
    let pipe = UndrainedPipe {
        bytes: pipe_bytes,
        waiting: waiting_bytes,
        nonblocking,
        stdout_too: false,
    };
    let args: Vec<&str> = global_args
        .iter()
        .copied()
        .chain(["run", "--verify", "--base-env=minimal"])
        .collect();
    let Some((status, delivered)) = run_against_an_undrained_pipe(&args, &script, pipe) else {
        return;
    };
    assert_eq!(
        status.code(),
        Some(HERMIT_INTERNAL_FAILURE_EXIT),
        "the report replaced the rejected first run's status\nstderr:\n{delivered}"
    );
    assert!(
        delivered.contains("First run during --verify"),
        "the disposition must arrive ahead of the report\nstderr:\n{delivered}"
    );
    assert!(
        delivered.contains(":: The run 1 guest was terminated by signal 10 (SIGUSR1)."),
        "the report's first line fits in the pipe and must arrive\nstderr:\n{delivered}"
    );
}

/// Twenty checks for a path whose name is 300 bytes long, so that each of a
/// run's last records is long.
fn twenty_long_path_checks() -> String {
    let path = format!("/{}", "p".repeat(300));
    format!("for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do [ -e {path} ]; done")
}

/// A pipe of `bytes` with `waiting` bytes already in it, which nobody reads
/// until hermit exits.
struct UndrainedPipe {
    bytes: i32,
    waiting: usize,
    nonblocking: bool,
    /// Whether hermit's stdout is the pipe too, rather than `/dev/null`.
    stdout_too: bool,
}

/// Run hermit with `hermit_args` and then `-- /bin/sh -c script`, its stderr
/// `pipe`, and return its status, within a bound, and what reached the pipe.
///
/// `None`, with the reason on stderr, when the host cannot give the pipe its
/// size: Linux refuses to grow a pipe (`F_SETPIPE_SZ` fails with EPERM) for
/// an unprivileged user over its per-user pipe-page limit
/// (`fs.pipe-user-pages-soft`), whose pipes have two pages, or past
/// `fs.pipe-max-size`. The test's geometry needs the exact size, so it is
/// then not run, rather than failed at its own setup. Any other mismatch
/// still fails.
fn run_against_an_undrained_pipe(
    hermit_args: &[&str],
    script: &str,
    pipe: UndrainedPipe,
) -> Option<(std::process::ExitStatus, String)> {
    use std::os::unix::io::FromRawFd;

    let mut fds = [0i32; 2];
    // SAFETY: `fds` holds two descriptors.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    let (read_fd, write_fd) = (fds[0], fds[1]);
    // SAFETY: fcntl on descriptors this test owns.
    let (set, set_errno, capacity) = unsafe {
        let set = libc::fcntl(write_fd, libc::F_SETPIPE_SZ, pipe.bytes);
        let set_errno = std::io::Error::last_os_error().raw_os_error();
        (set, set_errno, libc::fcntl(write_fd, libc::F_GETPIPE_SZ))
    };
    if set < 0 && set_errno == Some(libc::EPERM) && capacity < pipe.bytes {
        let limit = |name: &str| {
            fs::read_to_string(format!("/proc/sys/fs/{name}")).map_or_else(
                |_| String::from("unreadable"),
                |value| value.trim().to_owned(),
            )
        };
        eprintln!(
            "SKIPPED: this host cannot give the test's pipe {} bytes: F_SETPIPE_SZ \
             failed with EPERM and the pipe has {capacity}; the user is over its \
             per-user pipe-page limit or the size is past the maximum \
             (fs.pipe-user-pages-soft = {}, fs.pipe-user-pages-hard = {}, \
             fs.pipe-max-size = {})",
            pipe.bytes,
            limit("pipe-user-pages-soft"),
            limit("pipe-user-pages-hard"),
            limit("pipe-max-size"),
        );
        // SAFETY: both descriptors are this test's and not yet owned.
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        return None;
    }
    assert_eq!(capacity, pipe.bytes, "pipe capacity");
    let waiting = vec![b'.'; pipe.waiting];
    // SAFETY: `waiting` is valid for its length; it fits in the empty pipe.
    let written = unsafe { libc::write(write_fd, waiting.as_ptr().cast(), waiting.len()) };
    assert_eq!(written, pipe.waiting as isize, "bytes waiting in the pipe");
    if pipe.nonblocking {
        // SAFETY: as above.
        unsafe {
            let flags = libc::fcntl(write_fd, libc::F_GETFL);
            libc::fcntl(write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    // SAFETY: each descriptor is owned by exactly one of these.
    let (mut reader, writer) = unsafe {
        (
            fs::File::from_raw_fd(read_fd),
            fs::File::from_raw_fd(write_fd),
        )
    };

    let mut command = Command::new(hermit_test::hermit_binary());
    command
        .args(hermit_args)
        .args(["--", "/bin/sh", "-c", script]);
    // Before stdio: in the marked integration gate this rebuilds `command`,
    // which keeps its arguments but not its stdio.
    hermit_test::configure_guest_execution(&mut command);
    let stdout = if pipe.stdout_too {
        Stdio::from(writer.try_clone().expect("failed to clone the pipe"))
    } else {
        Stdio::null()
    };
    // `--verify` reads stdin to its end before run 1.
    command.stdin(Stdio::null()).stdout(stdout).stderr(writer);
    let started = Instant::now();
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start hermit: {error}"));
    // Dropped so that only hermit holds the write end.
    drop(command);
    // Bounded, so a report that waits is a failure rather than a hang.
    let deadline = Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break Some(status),
            None if started.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let mut delivered = Vec::new();
    reader
        .read_to_end(&mut delivered)
        .expect("failed to read the delivered output");
    let delivered = String::from_utf8_lossy(&delivered).into_owned();
    let status = status.unwrap_or_else(|| {
        panic!("hermit had not exited after {deadline:?}\ndelivered:\n{delivered}")
    });
    Some((status, delivered))
}
