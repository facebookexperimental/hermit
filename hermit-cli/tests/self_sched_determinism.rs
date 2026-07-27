/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for host scheduler accounting in /proc/self/sched.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: &'static [&'static str],
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn required_program(case: &ProgramCase) -> PathBuf {
    case.candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "required program {} is missing; expected one of {:?}",
                case.name, case.candidates
            )
        })
}

fn assert_l2(case: &ProgramCase) {
    let program = required_program(case);
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log",
            "DEBUG",
            "run",
            "--backend=ptrace",
            "--strict",
            "--verify",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&program)
        .args(case.args);

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "{} failed strict verification ({rendered})\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
        output.status,
    );
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "{} omitted Hermit's verification marker ({rendered})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
    );
}

fn read_self_sched() -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "--log",
        "ERROR",
        "run",
        "--backend=ptrace",
        "--strict",
        "--panic-on-unsupported-syscalls",
        "--base-env=minimal",
        "--",
        "/bin/cat",
        "/proc/self/sched",
    ]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "sched read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("sched should be UTF-8")
}

fn field_value<'a>(contents: &'a str, name: &str) -> &'a str {
    contents
        .lines()
        .find_map(|line| {
            let (label, value) = line.split_once(':')?;
            (label.trim() == name).then(|| value.trim())
        })
        .unwrap_or_else(|| panic!("sched omitted {name}:\n{contents}"))
}

#[test]
fn self_sched_consumers_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let sched = read_self_sched();
    for field in ["se.exec_start", "se.vruntime", "se.sum_exec_runtime"] {
        assert_eq!(field_value(&sched, field), "0.000000");
    }
    for field in [
        "nr_switches",
        "se.avg.load_avg",
        "se.avg.last_update_time",
        "clock-delta",
    ] {
        assert_eq!(field_value(&sched, field), "0");
    }

    let cases = [
        ProgramCase {
            name: "grep execution start",
            candidates: &["/usr/bin/grep", "/bin/grep"],
            args: &["^se.exec_start", "/proc/self/sched"],
        },
        ProgramCase {
            name: "awk virtual runtime",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["/^se.vruntime/ { print }", "/proc/self/sched"],
        },
        ProgramCase {
            name: "sed execution runtime",
            candidates: &["/bin/sed", "/usr/bin/sed"],
            args: &["-n", "/^se.sum_exec_runtime/p", "/proc/self/sched"],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
