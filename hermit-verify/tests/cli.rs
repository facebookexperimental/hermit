/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

const PASSING_SCHEDULE: &str = "flaky_cas_sequence_schedules-passing.json";
const FAILING_SCHEDULE: &str = "flaky_cas_sequence_schedules-failing.json";

fn schedule(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-verify should be inside the repository")
        .join("hermit-cli/test-resources")
        .join(name)
}

fn hermit_verify(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hermit-verify"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit-verify with {args:?}: {error}"))
}

fn assert_success(output: &Output, command: &str) {
    assert!(
        output.status.success(),
        "{command} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn help_lists_verification_and_trace_commands() {
    let output = hermit_verify(&["--help"]);
    assert_success(&output, "hermit-verify --help");
    let help = String::from_utf8(output.stdout).expect("help should be UTF-8");

    assert!(help.contains("Usage: hermit-verify [OPTIONS] <COMMAND>"));
    for command in [
        "run",
        "trace-replay",
        "chaos-replay",
        "sched-trace",
        "chaos-stress",
    ] {
        assert!(help.contains(command), "missing {command:?} in:\n{help}");
    }
}

#[test]
fn schedule_diff_reports_stable_trace_distances() {
    let passing = schedule(PASSING_SCHEDULE);
    let failing = schedule(FAILING_SCHEDULE);
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-verify"))
        .args(["sched-trace", "diff"])
        .arg(passing)
        .arg(failing)
        .output()
        .expect("failed to compare schedule traces");
    assert_success(&output, "hermit-verify sched-trace diff");

    let stdout = String::from_utf8(output.stdout).expect("diff stdout should be UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("diff stderr should be UTF-8");
    assert!(stdout.contains("first schedule length: 432"));
    assert!(stdout.contains("second schedule length: 433"));
    assert!(
        stderr.contains("Swap distance  = 201"),
        "unexpected diff:\n{stderr}"
    );
    assert!(
        stderr.contains("Edit distance = 270"),
        "unexpected diff:\n{stderr}"
    );
}

#[test]
fn schedule_inspect_reports_context_switches_per_thread() {
    let passing = schedule(PASSING_SCHEDULE);
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-verify"))
        .args(["sched-trace", "inspect"])
        .arg(passing)
        .output()
        .expect("failed to inspect schedule trace");
    assert_success(&output, "hermit-verify sched-trace inspect");

    let stdout = String::from_utf8(output.stdout).expect("inspect stdout should be UTF-8");
    for expected in [
        "Schedule length: 432",
        "Context-switch/preemption events: 7",
        "tid 3: 3 preemptions",
        "tid 5: 2 preemptions",
        "tid 7: 1 preemptions",
        "tid 9: 1 preemptions",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
}

#[test]
fn schedule_print_filters_thread_and_strips_times() {
    let passing = schedule(PASSING_SCHEDULE);
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-verify"))
        .args([
            "sched-trace",
            "print",
            "--indices",
            "--tid",
            "3",
            "--strip-times",
        ])
        .arg(passing)
        .output()
        .expect("failed to print schedule trace");
    assert_success(&output, "hermit-verify sched-trace print");

    let stdout = String::from_utf8(output.stdout).expect("print stdout should be UTF-8");
    assert!(
        stdout.starts_with("(0,0)     (tid3"),
        "unexpected output:\n{stdout}"
    );
    assert!(
        !stdout.contains(" time="),
        "times were not stripped:\n{stdout}"
    );
    for line in stdout.lines().filter(|line| !line.is_empty()) {
        assert!(line.contains("(tid3"), "unexpected thread in line: {line}");
    }
}
