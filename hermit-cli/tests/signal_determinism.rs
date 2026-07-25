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

const DETERMINISM_RUNS: usize = 5;

static HERMIT_SIGNAL_LOCK: Mutex<()> = Mutex::new(());
static SIGNAL_GUEST: OnceLock<PathBuf> = OnceLock::new();

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

fn run_signal_scenario(scenario: &str, expected_stdout: &str) {
    let _guard = hermit_signal_lock();
    let mut baseline = None;

    for iteration in 0..DETERMINISM_RUNS {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command.args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
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

#[test]
fn sigalrm_itimer_delivery_is_deterministic() {
    run_signal_scenario(
        "itimer-delivery",
        "alarm delivered\nalarm pending=1 phase=2 deliveries=1\n",
    );
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
