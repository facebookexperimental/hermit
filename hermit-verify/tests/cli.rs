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

/// A stand-in for Hermit whose Nth run ends as the Nth word of OUTCOMES says:
/// the guest passing or failing, the guest killed by a signal, or one of
/// Hermit's own outcomes with the stderr line Hermit prints for it.
const SCRIPTED_HERMIT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
n=$(($(cat "$dir/runs" 2>/dev/null || echo 0) + 1))
echo "$n" > "$dir/runs"
set -- $OUTCOMES
eval "outcome=\${$n:-pass}"
case $outcome in
pass) exit 0 ;;
fail) exit 1 ;;
killed) kill -KILL $$ ;;
skid)
  echo 'HERMIT_SKID_OVERSHOOT rcb_actual=240935 rcb_target=240682 skid_margin=1000 overshoot=253' >&2
  echo 'HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=1' >&2
  exit 122 ;;
internal)
  echo 'HERMIT_INTERNAL_FAILURE class=container-child-panic' >&2
  exit 125 ;;
esac
"#;

/// `hermit-verify chaos-stress` of `/bin/true` against [`SCRIPTED_HERMIT`]
/// with OUTCOMES, and the number of Hermit runs it made.
fn chaos_stress_with(outcomes: &str) -> (Output, u32) {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().expect("failed to create a directory for the stand-in");
    let hermit = directory.path().join("hermit");
    std::fs::write(&hermit, SCRIPTED_HERMIT).unwrap();
    std::fs::set_permissions(&hermit, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-verify"))
        .arg("--hermit-bin")
        .arg(&hermit)
        .args(["chaos-stress", "--max-iterations-count", "5", "/bin/true"])
        .env("OUTCOMES", outcomes)
        .output()
        .expect("failed to run hermit-verify chaos-stress");
    let runs = std::fs::read_to_string(directory.path().join("runs"))
        .map_or(0, |runs| runs.trim().parse().expect("run count"));
    (output, runs)
}

#[test]
fn chaos_stress_retries_a_skid_refusal_instead_of_counting_it() {
    let (output, runs) = chaos_stress_with("pass skid skid pass");
    assert!(
        !output.status.success(),
        "a refusal counted as a failing guest:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(runs, 7, "5 iterations and 2 retries");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("5 passing and 0 failing runs; 2 skid refusals retried"),
        "{stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SKID-RETRY run #2 attempt 2: HERMIT_POLICY_REFUSAL"),
        "{stderr}"
    );
}

#[test]
fn chaos_stress_finds_a_race_between_guest_outcomes() {
    let (output, runs) = chaos_stress_with("skid pass killed");
    assert_success(&output, "hermit-verify chaos-stress");
    assert_eq!(runs, 3);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Found a passing and a failing run; 1 skid refusals retried"),
        "{stdout}"
    );
}

#[test]
fn chaos_stress_stops_when_hermit_itself_fails() {
    let (output, runs) = chaos_stress_with("pass internal fail");
    assert!(
        !output.status.success(),
        "an internal failure counted as a failing guest"
    );
    assert_eq!(runs, 2);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Hermit itself ended the run with status 125")
            && stderr.contains("HERMIT_INTERNAL_FAILURE class=container-child-panic"),
        "{stderr}"
    );
}
