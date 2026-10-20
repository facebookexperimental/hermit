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

fn bounded_command_output(
    mut command: Command,
    label: &str,
    bound: Duration,
) -> (Output, bool, Duration) {
    hermit_test::configure_guest_execution(&mut command);
    // A typed PMU skid-overshoot refusal, and nothing else, runs it again.
    let (output, (timed_out, elapsed)) = hermit_test::hermit_output_with(label, || {
        let (output, timed_out, elapsed) = bounded_command_output_once(&mut command, label, bound);
        (output, (timed_out, elapsed))
    });
    (output, timed_out, elapsed)
}

/// One attempt of [`bounded_command_output`].
fn bounded_command_output_once(
    command: &mut Command,
    label: &str,
    bound: Duration,
) -> (Output, bool, Duration) {
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
            Ok(None) if started.elapsed() >= bound => {
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
        process_timed_out || elapsed >= bound,
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
mod dbt_sigkill_ends_every_thread_of_the_target {
    use super::*;

    // Each scenario retains all five runs and its exact output comparison,
    // while the per-test CPU limit applies to one scenario at a time.
    fn run_scenario(index: usize) {
        let (scenario, expected) = SIGKILL_SCENARIOS[index];
        run_signal_scenario_on_backend(Some("dbt"), scenario, expected, DETERMINISM_RUNS);
    }

    #[test]
    fn kill_single_thread() {
        run_scenario(0);
    }

    #[test]
    fn kill_multiple_threads() {
        run_scenario(1);
    }

    #[test]
    fn tgkill_single_thread() {
        run_scenario(2);
    }

    #[test]
    fn tgkill_multiple_threads() {
        run_scenario(3);
    }

    #[test]
    fn kill_sigchld_fence() {
        run_scenario(4);
    }

    #[test]
    fn kill_threaded_sigchld_fence() {
        run_scenario(5);
    }

    #[test]
    fn kill_pipe_eof_fence() {
        run_scenario(6);
    }

    #[test]
    fn kill_threaded_pipe_eof_fence() {
        run_scenario(7);
    }

    #[test]
    fn pidfd_after_leader_exit() {
        run_scenario(8);
    }

    #[test]
    fn pidfd_zombie() {
        run_scenario(9);
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
        // This is policy margin over censored five-second failures under load,
        // not a measured completion bound or a fix for the unresolved stall.
        // Other deadlock probes keep five seconds; per-test CPU/wall caps stay.
        let (output, timed_out, elapsed) =
            bounded_command_output(command, &label, Duration::from_secs(8));
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
        let (output, timed_out, elapsed) = bounded_command_output(command, &label, DEADLOCK_BOUND);
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

static DELIVERY_GUEST: OnceLock<PathBuf> = OnceLock::new();

/// tests/c/signal_delivery_check.c, built once: caught deliveries of known
/// shapes, from the thread itself or a sibling, each checking what Linux
/// guarantees about it.
fn delivery_guest() -> &'static Path {
    DELIVERY_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("signal-delivery-check-{}", std::process::id()));
            fs::create_dir_all(&build_root)
                .expect("failed to create signal delivery check build directory");
            let binary = build_root.join("signal_delivery_check");
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
                    "-pthread",
                ])
                .arg(repository.join("tests/c/signal_delivery_check.c"))
                .arg("-o")
                .arg(&binary);
            command_output(command, "signal delivery check guest compilation");
            binary
        })
        .as_path()
}

/// Runs `mode` of the delivery guest on ptrace twice:
/// - under `--strict --verify --verify-strict`, which must pass (a delivery
///   the kernel contradicts ends a strict run);
/// - under `--strict` with Detcore's delivery counters logged
///   (`RUST_LOG=detcore::signal_delivery=trace`).
///
/// Both runs must print the guest's `OK <mode>`. `extra` are more `run` flags
/// for both runs. Returns each thread's counter line, from `approved=` on, in
/// the order the threads exited, and each approval's trace line, from
/// `the delivery of` on.
fn delivery_log(mode: &str, extra: &[&str]) -> (Vec<String>, Vec<String>) {
    let (counters, approvals, _) = delivery_log_with(mode, extra, "detcore::signal_delivery=trace");
    (counters, approvals)
}

/// [`delivery_log`], with `rust_log` for the counted run, also returning
/// that run's whole log.
fn delivery_log_with(
    mode: &str,
    extra: &[&str],
    rust_log: &str,
) -> (Vec<String>, Vec<String>, String) {
    let expected = format!("OK {mode}\n");
    let mut verify = Command::new(hermit_test::hermit_binary());
    verify.args(["run", "--strict", "--verify", "--verify-strict"]);
    verify.args(extra).arg("--");
    verify.arg(delivery_guest()).arg(mode);
    let output = command_output(
        verify,
        &format!("delivery check {mode} under --verify-strict"),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);

    let log = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "signal-delivery-check-{}-{mode}.log",
        std::process::id()
    ));
    let _ = fs::remove_file(&log);
    let mut counted = Command::new(hermit_test::hermit_binary());
    counted
        .env("RUST_LOG", rust_log)
        .arg("--log-file")
        .arg(&log)
        .args(["run", "--strict"]);
    counted.args(extra).arg("--");
    counted.arg(delivery_guest()).arg(mode);
    let output = command_output(counted, &format!("delivery check {mode} with counters"));
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    let log_text = fs::read_to_string(&log).expect("read the counter log");
    let counters = log_text
        .lines()
        .filter_map(|line| line.split("signal deliveries: ").nth(1))
        .map(str::to_owned)
        .collect();
    let approvals = log_text
        .lines()
        .filter(|line| line.contains("is approved"))
        .filter_map(|line| line.split("signal-state copy: ").nth(1))
        .map(str::to_owned)
        .collect();
    (counters, approvals, log_text)
}

/// For the multithreaded modes: [`assert_counters`], with Detcore's warnings
/// and errors logged too, of which there must be none: no copy diverged
/// after the last delivery (`SIGNAL_STATE_COPY_DIVERGED`), and no signal was
/// refused.
fn assert_mt_counters(mode: &str, expected: &[&str]) {
    let (counters, _, log) =
        delivery_log_with(mode, &[], "detcore=warn,detcore::signal_delivery=trace");
    assert_eq!(counters, expected, "mode {mode}");
    let noise: Vec<&str> = log
        .lines()
        .filter(|line| line.contains(" WARN ") || line.contains(" ERROR "))
        .collect();
    assert!(noise.is_empty(), "mode {mode}: {noise:#?}");
}

/// [`delivery_log`]'s counter lines.
fn delivery_counters(mode: &str) -> Vec<String> {
    delivery_log(mode, &[]).0
}

/// The counter line of a thread whose one caught delivery is approved, at
/// the signal's own stop (not held), with Detcore's syscall result compared
/// with `rax`, and which no signal reached unreported.
const ONE_APPROVAL: &str = "approved=1 held=0 result_checked=1 mismatched=0 interruption_from_registers=0 skipped={} unreported={}";

/// The counter line of a thread whose one signal is not caught.
const NOT_CAUGHT: &str = r#"approved=0 held=0 result_checked=0 mismatched=0 interruption_from_registers=0 skipped={"not caught": 1} unreported={}"#;

fn assert_counters(mode: &str, expected: &[&str]) {
    let _guard = hermit_signal_lock();
    assert_eq!(delivery_counters(mode), expected, "mode {mode}");
}

#[test]
fn ptrace_delivery_check_approves_a_raised_signal() {
    assert_counters("raise", &[ONE_APPROVAL]);
}

#[test]
fn ptrace_delivery_check_approves_a_signal_that_interrupts_pause() {
    assert_counters("pause", &[ONE_APPROVAL]);
}

#[test]
fn ptrace_delivery_check_approves_a_signal_that_interrupts_nanosleep() {
    assert_counters("nanosleep", &[ONE_APPROVAL]);
}

#[test]
fn ptrace_delivery_check_approves_a_signal_that_interrupts_a_pipe_read() {
    assert_counters("read", &[ONE_APPROVAL]);
}

#[test]
fn ptrace_delivery_check_approves_a_signal_whose_handler_restarts_a_pipe_read() {
    assert_counters("read-restart", &[ONE_APPROVAL]);
}

/// A spin in user code, which takes the signal at a PMU timer preemption, so
/// test.pmu_integration_cases runs it: no syscall result to compare.
#[test]
fn ptrace_delivery_check_approves_a_signal_to_a_thread_in_user_code() {
    assert_counters(
        "spin",
        &[
            "approved=1 held=0 result_checked=0 mismatched=0 interruption_from_registers=0 skipped={} unreported={}",
        ],
    );
}

#[test]
fn ptrace_delivery_check_approves_a_signal_admitted_by_sigsuspend() {
    assert_counters("suspend", &[ONE_APPROVAL]);
}

/// Inside sigsuspend, SIGSEGV (a signal Linux can force-deliver) is blocked
/// in the thread's own mask, which the frame saves, and not in the
/// temporary mask. The check leaves SIGSEGV out of the frame's comparison
/// only and approves the delivery (A5 step 3.2b). Before, a forced bit in
/// one mask only left the delivery unchecked
/// (`skipped={"forced bit in one mask only": 1}`).
#[test]
fn ptrace_delivery_check_approves_with_a_forced_bit_in_the_frame_mask_only() {
    assert_counters("suspend-forced-frame", &[ONE_APPROVAL]);
}

/// As `..._in_the_frame_mask_only`, with SIGSEGV blocked in the temporary
/// mask and not in the thread's own: left out of the current mask's
/// comparison only.
#[test]
fn ptrace_delivery_check_approves_with_a_forced_bit_in_the_current_mask_only() {
    assert_counters("suspend-forced-current", &[ONE_APPROVAL]);
}

/// A blocked SIGSYS with a kernel si_code, which the backend takes at the
/// steps of syscalls it skips (Hermit's emulated sched_yield, then the
/// unblocking rt_sigprocmask, which Detcore injects) and passes back
/// unreported each time, while the mask blocks it, so the kernel requeues
/// it: counted as two unreported passes by their reason (A5 step 3.2b). The
/// first pass gives the copy up (the signal could have entered a handler the
/// copy did not see), so the delivery at the unblock is not checked
/// (`no copy`). The guest checks that the signal stays pending through
/// sched_yield and its handler runs once, at the unblock, as natively.
#[test]
fn ptrace_delivery_check_counts_a_signal_passed_on_unreported_at_a_syscall_skip() {
    assert_counters(
        "skip-bypass",
        &[
            r#"approved=0 held=0 result_checked=0 mismatched=0 interruption_from_registers=0 skipped={"no copy": 1} unreported={"during a syscall skip": 2}"#,
        ],
    );
}

#[test]
fn ptrace_delivery_check_approves_a_queued_realtime_signal() {
    assert_counters("realtime", &[ONE_APPROVAL]);
}

/// SA_RESETHAND: the first SIGWINCH is approved and runs the handler; the
/// second takes the default action, ignoring it, and is not caught.
#[test]
fn ptrace_delivery_check_approves_an_sa_resethand_delivery_then_not_the_default() {
    assert_counters(
        "resethand",
        &[
            r#"approved=1 held=0 result_checked=1 mismatched=0 interruption_from_registers=0 skipped={"not caught": 1} unreported={}"#,
        ],
    );
}

/// Two sends of a blocked standard signal are one pending signal: one
/// approval, at the unblock.
#[test]
fn ptrace_delivery_check_approves_two_coalesced_sends_once() {
    assert_counters("coalesce", &[ONE_APPROVAL]);
}

/// Two queued sends of an unblocked realtime signal: each is approved as its
/// sigqueue returns, with that call's result compared.
#[test]
fn ptrace_delivery_check_approves_two_direct_realtime_sends() {
    assert_counters(
        "realtime-direct",
        &[
            "approved=2 held=0 result_checked=2 mismatched=0 interruption_from_registers=0 skipped={} unreported={}",
        ],
    );
}

/// A write that moved bytes before the signal returns the count (the guest
/// checks it is the pipe's capacity), and the delivery is approved at that
/// write's exit, with its count compared.
#[test]
fn ptrace_delivery_check_approves_a_signal_after_a_partial_write() {
    let _guard = hermit_signal_lock();
    let (counters, approvals) = delivery_log("partial-write", &[]);
    assert_eq!(counters, [ONE_APPROVAL]);
    assert!(
        approvals.len() == 1 && approvals[0].contains("Return { syscall: 1, result: "),
        "{approvals:?}"
    );
}

/// With sigreturn reports and precise timers off (--imprecise-timers),
/// checks continue.
#[test]
fn ptrace_delivery_check_approves_under_imprecise_timers() {
    let _guard = hermit_signal_lock();
    assert_eq!(
        delivery_log("pause", &["--imprecise-timers"]).0,
        [ONE_APPROVAL]
    );
}

#[test]
fn ptrace_delivery_check_approves_an_sa_nodefer_delivery() {
    assert_counters("nodefer", &[ONE_APPROVAL]);
}

/// An ignored signal is never offered for approval: Detcore checks caught
/// deliveries only.
#[test]
fn ptrace_delivery_check_never_offers_an_ignored_signal() {
    assert_counters("ignored", &[NOT_CAUGHT]);
}

/// Two signals pending at one exit: the second is delivered right after the
/// first one's frame is built, with no syscall result of its own to compare.
#[test]
fn ptrace_delivery_check_approves_two_signals_at_one_exit() {
    assert_counters(
        "two-pending",
        &[
            "approved=2 held=0 result_checked=1 mismatched=0 interruption_from_registers=0 skipped={} unreported={}",
        ],
    );
}

/// A blocked signal does not end a sleep, and is delivered, once, at the
/// unblock.
#[test]
fn ptrace_delivery_check_approves_a_signal_delivered_at_its_unblock() {
    assert_counters("blocked-sleep", &[ONE_APPROVAL]);
}

/// A default-ignored signal is never offered for approval.
#[test]
fn ptrace_delivery_check_never_offers_a_default_ignored_signal() {
    assert_counters("default-ignored", &[NOT_CAUGHT]);
}

/// A default-fatal signal is never offered for approval: the child's SIGTERM
/// and the parent's default-ignored SIGCHLD are each not caught.
#[test]
fn ptrace_delivery_check_never_offers_a_default_fatal_signal() {
    assert_counters("default-fatal", &[NOT_CAUGHT, NOT_CAUGHT]);
}

/// Two queued instances of one realtime signal are delivered in order at the
/// unblock. The second comes after the first handler ran, so its
/// interruption is the registers' answer.
#[test]
fn ptrace_delivery_check_approves_two_queued_realtime_instances_in_order() {
    assert_counters(
        "realtime-two",
        &[
            "approved=2 held=0 result_checked=1 mismatched=0 interruption_from_registers=1 skipped={} unreported={}",
        ],
    );
}

/// A child's signal interrupts its parent's wait4, which SA_RESTART restarts:
/// the delivery is approved at the interrupted wait4's exit, with its
/// ERESTARTSYS result compared, and the restarted wait reaps the child. The
/// parent's SIGCHLD is not caught.
#[test]
fn ptrace_delivery_check_approves_a_signal_that_interrupts_a_restarted_child_wait() {
    let _guard = hermit_signal_lock();
    let (counters, approvals) = delivery_log("child-wait-restart", &[]);
    assert_eq!(
        counters,
        [
            r#"approved=1 held=0 result_checked=1 mismatched=0 interruption_from_registers=0 skipped={"not caught": 1} unreported={}"#,
        ]
    );
    assert!(
        approvals.len() == 1 && approvals[0].contains("Return { syscall: 61, result: -512 }"),
        "{approvals:?}"
    );
}

/// A handler frame that cannot be written: SIGUSR1 is approved, the kernel
/// raises SIGSEGV with SA_RESETHAND applied, and the copy, which cannot tell
/// the faulted frame from a handler, is given up quietly by the SIGSEGV
/// delivery's reconciliation. Nothing is refused.
#[test]
fn ptrace_delivery_check_gives_the_copy_up_quietly_after_a_frame_fault() {
    assert_counters(
        "frame-fault",
        &[
            r#"approved=1 held=0 result_checked=1 mismatched=0 interruption_from_registers=0 skipped={"copy given up at this delivery": 1} unreported={}"#,
        ],
    );
}

/// The same fault with SA_NODEFER and an empty sa_mask, where the handler's
/// mask equals the frame's, so the copy survives: the SIGSEGV delivery at the
/// same syscall exit is approved too, its interruption taken from the
/// registers (the frame was not built). Before, that stop was a false
/// contradiction.
#[test]
fn ptrace_delivery_check_approves_the_sigsegv_of_a_frame_fault_under_sa_nodefer() {
    let _guard = hermit_signal_lock();
    let (counters, approvals) = delivery_log("frame-fault-nodefer", &[]);
    assert_eq!(
        counters,
        [
            "approved=2 held=0 result_checked=1 mismatched=0 interruption_from_registers=1 skipped={} unreported={}"
        ]
    );
    assert!(
        approvals.len() == 2 && approvals[1].contains("KernelRun { syscall: 234 }"),
        "{approvals:?}"
    );
}

/// A fault in a child that blocks a caught SIGSEGV: Linux unblocks it and
/// resets the action to the default, and the child dies. Its copy is given up
/// unchecked; the parent's SIGCHLD is not caught.
#[test]
fn ptrace_delivery_check_gives_up_a_forced_signal_the_copy_blocks() {
    assert_counters(
        "forced-blocked",
        &[
            r#"approved=0 held=0 result_checked=0 mismatched=0 interruption_from_registers=0 skipped={"forced while blocked": 1} unreported={}"#,
            NOT_CAUGHT,
        ],
    );
}

/// A fault in a child that ignores SIGSEGV: Linux resets the action to the
/// default and the child dies. Its copy is given up unchecked; the parent's
/// SIGCHLD is not caught.
#[test]
fn ptrace_delivery_check_gives_up_a_forced_signal_the_copy_ignores() {
    assert_counters(
        "forced-ignored",
        &[
            r#"approved=0 held=0 result_checked=0 mismatched=0 interruption_from_registers=0 skipped={"forced while ignored": 1} unreported={}"#,
            NOT_CAUGHT,
        ],
    );
}
/// In an `--log info` run of `mode` of the delivery guest, the first DETLOG
/// syscall entry of `first` must come before that of `second`, each given as
/// (Detcore thread ID, syscall name): the proof that a multithreaded case hit
/// the window it names. The main thread is dtid 3 and its sibling dtid 5. The
/// run is separate from the counted ones; its schedule is the same.
fn assert_detlog_order(mode: &str, first: (u32, &str), second: (u32, &str)) {
    let log = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "signal-delivery-order-{}-{mode}.log",
        std::process::id()
    ));
    let _ = fs::remove_file(&log);
    let mut run = Command::new(hermit_test::hermit_binary());
    run.args(["--log", "info", "--log-file"])
        .arg(&log)
        .args(["run", "--strict", "--"]);
    run.arg(delivery_guest()).arg(mode);
    let output = command_output(run, &format!("delivery check {mode} with DETLOG"));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("OK {mode}\n")
    );
    let text = fs::read_to_string(&log).expect("read the DETLOG log");
    let entry = |(dtid, name): (u32, &str)| {
        let needle = format!("dtid {dtid}] inbound syscall: {name}(");
        text.lines()
            .position(|line| line.contains(&needle))
            .unwrap_or_else(|| panic!("no dtid {dtid} {name} entry in the {mode} DETLOG"))
    };
    assert!(
        entry(first) < entry(second),
        "{mode}: {first:?} must come before {second:?} in DETLOG"
    );
}

/// A sibling's thread-directed SIGUSR1 that reaches the main thread before
/// its sigsuspend starts, while its own mask blocks the signal: the wait's
/// temporary mask admits it, and Detcore's check approves the delivery at
/// the wait's ERESTARTNOHAND, as the kernel delivers it. The DETLOG order
/// shows the send finished before the wait began.
#[test]
fn ptrace_delivery_check_approves_a_sibling_signal_pending_before_sigsuspend() {
    let _guard = hermit_signal_lock();
    assert_mt_counters("mt-suspend", &[ONE_APPROVAL]);
    assert_detlog_order("mt-suspend", (5, "tgkill"), (3, "rt_sigsuspend"));
}

/// The same, with the sibling's send after a 20 ms sleep, so that it reaches
/// the main thread during its sigsuspend (DETLOG: the wait began before the
/// send).
#[test]
fn ptrace_delivery_check_approves_a_sibling_signal_that_arrives_during_sigsuspend() {
    let _guard = hermit_signal_lock();
    assert_mt_counters("mt-suspend-late", &[ONE_APPROVAL]);
    assert_detlog_order("mt-suspend-late", (3, "rt_sigsuspend"), (5, "tgkill"));
}

/// The arrived-first cases (A5 step 3.2b, https://github.com/rrnewton/hermit/issues/4526):
/// a sibling's SIGUSR1, caught and admitted by the main thread's own mask,
/// reaches the main thread at the entry of `call` (a `mt-arrived-first-*`
/// mode), before Hermit's first injection for it, a kernel-access probe of
/// the call's mask or action. The scheduler, not host timing, places the
/// send there (the guest's comment says how). The probe's step stops for the
/// signal before it runs, so the backend holds the signal and reports it
/// after the callback (`held`). The call restarts once the signal is
/// delivered (Detcore's `-ERESTARTNOINTR`, -513, which is what the held
/// delivery is approved against), as if the signal had come before the
/// call: the handler runs under the thread's own mask, and the call then
/// runs (a wait then waits for the later SIGUSR2). The guest checks that
/// outcome against Linux's rules; before, the call returned EINTR, from a
/// wait whose temporary mask the handler never saw, and from
/// `rt_sigprocmask` and `rt_sigaction`, none of which a Linux run produces.
/// `second` is the later SIGUSR2's approval, and `held` the counter's
/// `held=` (a ppoll's SIGUSR2 is held too).
fn assert_arrived_first(call: &str, held: u32, first: &str, second: &str) {
    let _guard = hermit_signal_lock();
    let mode = format!("mt-arrived-first-{call}");
    let (counters, approvals, log) =
        delivery_log_with(&mode, &[], "detcore=warn,detcore::signal_delivery=trace");
    assert_eq!(
        counters,
        [format!(
            "approved=2 held={held} result_checked=2 mismatched=0 interruption_from_registers=0 skipped={{}} unreported={{}}"
        )],
        "mode {mode}"
    );
    assert_eq!(
        approvals,
        [
            format!("the delivery of signal 10 is approved, interrupting {first}"),
            format!("the delivery of signal 12 is approved, interrupting {second}"),
        ],
        "mode {mode}"
    );
    let noise: Vec<&str> = log
        .lines()
        .filter(|line| line.contains(" WARN ") || line.contains(" ERROR "))
        .collect();
    assert!(noise.is_empty(), "mode {mode}: {noise:#?}");
}

#[test]
fn ptrace_delivery_check_restarts_a_sigsuspend_a_signal_reaches_at_its_entry() {
    assert_arrived_first(
        "suspend",
        1,
        "Return { syscall: 130, result: -513 }",
        "Return { syscall: 130, result: -514 }",
    );
}

#[test]
fn ptrace_delivery_check_restarts_a_ppoll_a_signal_reaches_at_its_entry() {
    assert_arrived_first(
        "ppoll",
        2,
        "Return { syscall: 271, result: -513 }",
        "Return { syscall: 271, result: -514 }",
    );
}

#[test]
fn ptrace_delivery_check_restarts_a_pselect6_a_signal_reaches_at_its_entry() {
    assert_arrived_first(
        "pselect",
        1,
        "Return { syscall: 270, result: -513 }",
        "Return { syscall: 270, result: -514 }",
    );
}

#[test]
fn ptrace_delivery_check_restarts_an_rt_sigprocmask_a_signal_reaches_at_its_entry() {
    assert_arrived_first(
        "sigprocmask",
        1,
        "Return { syscall: 14, result: -513 }",
        "Return { syscall: 202, result: -512 }",
    );
}

#[test]
fn ptrace_delivery_check_restarts_an_rt_sigaction_a_signal_reaches_at_its_entry() {
    assert_arrived_first(
        "sigaction",
        1,
        "Return { syscall: 13, result: -513 }",
        "Return { syscall: 202, result: -512 }",
    );
}

/// Must not deliver: a sibling's SIGUSR1 that the wait's temporary mask also
/// blocks stays pending through sigsuspend, which the sibling's next signal,
/// an admitted SIGWINCH, ends. Two approvals: SIGWINCH at the wait, then
/// SIGUSR1 at the unblock.
#[test]
fn ptrace_delivery_check_does_not_deliver_a_sibling_signal_the_wait_blocks() {
    let _guard = hermit_signal_lock();
    assert_mt_counters(
        "mt-suspend-blocked",
        &[
            "approved=2 held=0 result_checked=2 mismatched=0 interruption_from_registers=0 skipped={} unreported={}",
        ],
    );
}

/// A process-directed SIGUSR1 from a sibling that admits it, while the main
/// thread blocks it and waits in pthread_join, runs its handler on the
/// sibling, under the sibling's mask, as Linux's complete_signal chooses. (A sender that blocks a process-directed signal
/// is refused: https://github.com/rrnewton/hermit/issues/4034.)
#[test]
fn ptrace_delivery_check_approves_a_process_directed_signal_on_the_thread_that_admits_it() {
    let _guard = hermit_signal_lock();
    assert_mt_counters("mt-process-to-sibling", &[ONE_APPROVAL]);
}

/// A thread-directed SIGUSR1 to the main thread, which blocks it, stays
/// pending there though the sibling admits it, and is delivered on the main
/// thread at its unblock.
#[test]
fn ptrace_delivery_check_keeps_a_thread_directed_signal_on_its_masked_target() {
    let _guard = hermit_signal_lock();
    assert_mt_counters("mt-thread-to-masked", &[ONE_APPROVAL]);
}

static STATE_KEPT_GUEST: OnceLock<PathBuf> = OnceLock::new();

/// tests/c/signal_state_kept.c, built once: SIGTRAP's state as each thread
/// sees it, across pthread_create and a joined thread.
fn state_kept_guest() -> &'static Path {
    STATE_KEPT_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("signal-state-kept-{}", std::process::id()));
            fs::create_dir_all(&build_root)
                .expect("failed to create signal state kept build directory");
            let binary = build_root.join("signal_state_kept");
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
                    "-pthread",
                ])
                .arg(repository.join("tests/c/signal_state_kept.c"))
                .arg("-o")
                .arg(&binary);
            command_output(command, "signal state kept guest compilation");
            binary
        })
        .as_path()
}

/// Runs the state-kept guest with SIGTRAP in `state`, natively and on ptrace
/// under `--strict --verify --verify-strict`, in both of its modes, and
/// compares Hermit's output with the native run's. The guest reads SIGTRAP's
/// action, its `rt_sigprocmask` bit and its procfs status bits: twice on one
/// thread (`single`), or before, in and after a joined thread (`threads`,
/// whose pthread_create blocks every signal around the clone with a call
/// Detcore injects). Under ptrace, Reverie's single-steps end with a forced
/// SIGTRAP, which resets a blocked or ignored SIGTRAP; Reverie keeps the
/// guest's SIGTRAP across them, and on a signal-action table no second thread
/// has used it runs each injected call under the guest's own mask, so the
/// guest's read of procfs `SigBlk` holds a blocked SIGTRAP as natively
/// (<https://github.com/rrnewton/hermit/issues/4485>).
///
/// Pinned limits, where Reverie keeps its step shield and the read sees a
/// blocked SIGTRAP unblocked: a caught SIGTRAP the guest blocks
/// (`caught_blocked`), every line of it; and, once a second thread has used
/// the table, every line read from then on, the thread's own and the one
/// after the join (`threads` mode, blocked states). Only SIGTRAP's bit in
/// `SigBlk` differs there; everything else matches the native run.
fn assert_state_kept(state: &str, blocked: bool, caught_blocked: bool) {
    let _guard = hermit_signal_lock();
    for mode in ["single", "threads"] {
        let mut native = Command::new(state_kept_guest());
        native.arg(mode).arg(state);
        let native = command_output(native, &format!("native signal_state_kept {mode} {state}"));
        let native = String::from_utf8_lossy(&native.stdout).into_owned();
        assert!(native.ends_with("done\n"), "native run: {native}");

        // The first line is read before any second thread exists.
        let shielded_from = if caught_blocked {
            Some(0)
        } else if blocked && mode == "threads" {
            Some(1)
        } else {
            None
        };
        let expected = match shielded_from {
            Some(from) => without_sigtrap_in_sigblk_from(&native, from),
            None => native.clone(),
        };

        let mut hermit = Command::new(hermit_test::hermit_binary());
        hermit.args(["run", "--strict", "--verify", "--verify-strict", "--"]);
        hermit.arg(state_kept_guest()).arg(mode).arg(state);
        let output = command_output(
            hermit,
            &format!("signal_state_kept {mode} {state} under --verify-strict"),
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "mode {mode}, state {state}, native output:\n{native}"
        );
    }
}

/// SIGTRAP's bit in a signal mask.
const SIGTRAP_MASK_BIT: u64 = 1 << (libc::SIGTRAP - 1);

/// The native output with SIGTRAP's bit cleared from the procfs `SigBlk`
/// fields of each line from line `from` (counted from 0) on, and nothing else
/// changed. The guest prints the whole mask with C's `%#llx`, which writes
/// zero as `0`.
fn without_sigtrap_in_sigblk_from(native: &str, from: usize) -> String {
    native
        .lines()
        .enumerate()
        .map(|(index, line)| {
            if index < from {
                return line.to_owned() + "\n";
            }
            let fields: Vec<String> = line
                .split(' ')
                .map(|field| {
                    if field == "SigBlk=1" {
                        "SigBlk=0".to_owned()
                    } else if let Some(hex) = field.strip_prefix("SigBlk-all=") {
                        let all = u64::from_str_radix(hex.trim_start_matches("0x"), 16)
                            .expect("a hex SigBlk")
                            & !SIGTRAP_MASK_BIT;
                        if all == 0 {
                            "SigBlk-all=0".to_owned()
                        } else {
                            format!("SigBlk-all={all:#x}")
                        }
                    } else {
                        field.to_owned()
                    }
                })
                .collect();
            fields.join(" ") + "\n"
        })
        .collect()
}

#[test]
fn ptrace_signal_state_kept_for_an_ignored_sigtrap() {
    assert_state_kept("ign", false, false);
}

#[test]
fn ptrace_signal_state_kept_for_an_ignored_blocked_sigtrap() {
    assert_state_kept("ign-blocked", true, false);
}

#[test]
fn ptrace_signal_state_kept_for_a_caught_sigtrap() {
    assert_state_kept("handler", false, false);
}

#[test]
fn ptrace_signal_state_kept_for_a_caught_blocked_sigtrap_but_its_sigblk_bit() {
    assert_state_kept("handler-blocked", true, true);
}

#[test]
fn ptrace_signal_state_kept_for_a_default_blocked_sigtrap() {
    assert_state_kept("dfl-blocked", true, false);
}

static SIGSEGV_STATE_KEPT_GUEST: OnceLock<PathBuf> = OnceLock::new();

/// tests/c/sigsegv_state_kept.c, built once: SIGSEGV's state after trapped
/// rdtsc and cpuid instructions, as each thread sees it.
fn sigsegv_state_kept_guest() -> &'static Path {
    SIGSEGV_STATE_KEPT_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("sigsegv-state-kept-{}", std::process::id()));
            fs::create_dir_all(&build_root)
                .expect("failed to create sigsegv state kept build directory");
            let binary = build_root.join("sigsegv_state_kept");
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
                    "-pthread",
                ])
                .arg(repository.join("tests/c/sigsegv_state_kept.c"))
                .arg("-o")
                .arg(&binary);
            command_output(command, "sigsegv state kept guest compilation");
            binary
        })
        .as_path()
}

/// Runs the SIGSEGV state guest with SIGSEGV in `state`, natively and on
/// ptrace under `--strict --verify --verify-strict`, in both of its modes,
/// and requires Hermit's output to equal the native run's. The guest reads
/// SIGSEGV's action, its `rt_sigprocmask` bit and its procfs status bits
/// after a trapped rdtsc, a trapped cpuid, an rdtsc first thing in a SIGUSR1
/// handler whose `sa_mask` blocks SIGSEGV, and an rdtsc right after that
/// handler returns (`single`), and also in a joined thread and after the join
/// (`threads`). Under ptrace, Hermit makes rdtsc and cpuid fault so that it
/// can emulate them, and Linux reports each fault with a forced SIGSEGV,
/// which resets a blocked or ignored SIGSEGV to `SIG_DFL` and unblocks it
/// before Reverie suppresses the signal. Reverie puts back what it reset
/// (<https://github.com/rrnewton/hermit/issues/4485>, A5 step 3.0b). On main,
/// every line after the first rdtsc read `action=dfl` and unblocked for the
/// ignored and blocked states, and the in-handler line read unblocked for
/// all six.
fn assert_sigsegv_state_kept(state: &str) {
    let _guard = hermit_signal_lock();
    for mode in ["single", "threads"] {
        let mut native = Command::new(sigsegv_state_kept_guest());
        native.arg(mode).arg(state);
        let native = command_output(native, &format!("native sigsegv_state_kept {mode} {state}"));
        let native = String::from_utf8_lossy(&native.stdout).into_owned();
        assert!(native.ends_with("done\n"), "native run: {native}");

        let mut hermit = Command::new(hermit_test::hermit_binary());
        hermit.args(["run", "--strict", "--verify", "--verify-strict", "--"]);
        hermit.arg(sigsegv_state_kept_guest()).arg(mode).arg(state);
        let output = command_output(
            hermit,
            &format!("sigsegv_state_kept {mode} {state} under --verify-strict"),
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            native,
            "mode {mode}, state {state}"
        );
    }
}

#[test]
fn ptrace_sigsegv_state_kept_for_an_ignored_sigsegv() {
    assert_sigsegv_state_kept("ign");
}

#[test]
fn ptrace_sigsegv_state_kept_for_an_ignored_blocked_sigsegv() {
    assert_sigsegv_state_kept("ign-blocked");
}

#[test]
fn ptrace_sigsegv_state_kept_for_a_caught_sigsegv() {
    assert_sigsegv_state_kept("handler");
}

#[test]
fn ptrace_sigsegv_state_kept_for_a_caught_blocked_sigsegv() {
    assert_sigsegv_state_kept("handler-blocked");
}

#[test]
fn ptrace_sigsegv_state_kept_for_a_default_sigsegv() {
    assert_sigsegv_state_kept("dfl");
}

#[test]
fn ptrace_sigsegv_state_kept_for_a_default_blocked_sigsegv() {
    assert_sigsegv_state_kept("dfl-blocked");
}
