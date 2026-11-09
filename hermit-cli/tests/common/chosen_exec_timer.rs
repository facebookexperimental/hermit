/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Actual post-exec native helpers need the new image's existing serial grant.
//! A timer entry on an ordinary physical signal record requires a real
//! rt_sigtimedwait take-back; an ordinary SI_TIMER exec case alone does not.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

use super::OWN_PID_NAMESPACE;
use super::assert_success;
use super::hermit_test;
use super::process_build_root;
use super::stderr;
use super::stdout;

static GUEST: OnceLock<PathBuf> = OnceLock::new();
const TAKE_BACK: &str = "exec flushed a POSIX timer's pending signal 10";

fn guest() -> &'static Path {
    GUEST.get_or_init(|| {
        let root = process_build_root("chosen-exec-timer");
        std::fs::create_dir_all(&root).expect("create chosen-exec-timer directory");
        let executable = root.join("chosen_exec_timer");
        let compiled = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/chosen_exec_timer.c"))
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("compile chosen-exec-timer fixture");
        assert!(
            compiled.status.success(),
            "chosen-exec-timer compilation failed: {compiled:?}"
        );
        executable
    })
}

#[derive(Clone, Copy, Debug)]
enum Run {
    Native,
    Namespace,
    Chosen,
}

fn run(mode: &str, kind: Run) -> Output {
    let mut command = Command::new("timeout");
    command.args(["--signal=TERM", "--kill-after=5s", "60s"]);
    match kind {
        Run::Native => {
            command.arg("unshare").args(OWN_PID_NAMESPACE);
        }
        Run::Namespace => {
            command.arg(hermit_test::hermit_binary());
        }
        Run::Chosen => {
            command
                .arg("unshare")
                .args(OWN_PID_NAMESPACE)
                .arg(hermit_test::hermit_binary());
        }
    }
    if !matches!(kind, Run::Native) {
        command.args([
            "--log=info",
            "run",
            "--base-env=minimal",
            "--max-timeslice=disabled",
            "--target-timeslice=1000",
            "--no-rcb-time",
            "--sched-heuristic=stickyrandom",
            "--sched-sticky-random-param=1",
            "--sched-seed=0",
            "--epoch=2026-01-01T00:00:00Z",
            "--timeout=45",
            "--summary",
        ]);
        if matches!(kind, Run::Chosen) {
            // Global --strict refuses --no-namespace's host-network policy.
            // The outer unshare still owns this fixture's network namespace,
            // and an explicit --pid-base requires Chosen IDs without fallback.
            command.args(["--no-namespace", "--pid-base=5000"]);
        } else {
            command.arg("--strict");
        }
        command.arg("--");
    }
    command.arg(guest()).arg(mode);
    if !matches!(kind, Run::Native) {
        hermit_test::configure_guest_execution(&mut command);
    }
    // PMU preemption is disabled. Do not turn a missing drain branch or a
    // refused Chosen run into a search for a luckier attempt or seed.
    hermit_test::hermit_output_attempts(
        &format!("post-exec timer take-back {kind:?}: {command:?}"),
        1,
        &mut command,
    )
}

fn check(mode: &str, pending: u8, take_back: usize) {
    let caller = if mode.ends_with("-nonleader") {
        "worker"
    } else {
        "leader-handler"
    };
    let expected = format!(
        "{mode} caller={caller} child_checked=1 post_exec_leader=1 pending_after_exec={pending} hits={pending}\n"
    );
    for kind in [Run::Native, Run::Namespace, Run::Chosen] {
        let output = run(mode, kind);
        assert_success(&output, &[mode]);
        let log = stderr(&output);
        assert_eq!(stdout(&output), expected, "{kind:?} {mode}: {log}");
        if !matches!(kind, Run::Native) {
            // Chosen cannot use global --strict, so reject the same delivery
            // contradiction that strict mode makes fatal before dropping its copy.
            assert!(
                !log.contains("the kernel contradicts the delivery of signal "),
                "{kind:?} {mode}: {log}"
            );
            assert_eq!(
                log.matches(TAKE_BACK).count(),
                take_back,
                "{kind:?} {mode}: stdout is not proof the post-exec native take-back ran: {log}"
            );
            assert!(
                !log.contains("got a host ID") && !log.contains("host-ID report(s) accepted"),
                "{kind:?} {mode}: {log}"
            );
        }
        match kind {
            Run::Chosen => assert!(
                log.contains("Guest process IDs: chosen by Hermit from 5000"),
                "{mode}: {log}"
            ),
            // Namespace is the default for `run` without --no-namespace or
            // --pid-base. Its summary deliberately has no guest-ID line.
            Run::Namespace => assert!(
                !log.contains("Guest process IDs:"),
                "{mode}: the ordinary Namespace control changed ID mode: {log}"
            ),
            Run::Native => {}
        }
    }
}

#[test]
fn nonleader_exec_takes_back_a_timer_proxy() {
    check("proxy-nonleader", 0, 1);
}

#[test]
fn same_leader_exec_takes_back_a_timer_proxy() {
    check("proxy-leader", 0, 1);
}

#[test]
fn exec_keeps_a_plain_signal_beside_the_timer() {
    for mode in ["plain-nonleader", "plain-leader"] {
        check(mode, 1, 0);
    }
}

#[test]
fn exec_lets_linux_flush_an_own_si_timer_record() {
    for mode in ["own-timer-nonleader", "own-timer-leader"] {
        check(mode, 0, 0);
    }
}
