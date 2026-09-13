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
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("signal-determinism");
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
        command.arg("run");
        if let Some(backend) = backend {
            command.arg(format!("--backend={backend}"));
        }
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
