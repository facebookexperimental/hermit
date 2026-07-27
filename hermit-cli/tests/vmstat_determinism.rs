/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for host VM accounting in /proc/vmstat.

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

fn read_vmstat() -> String {
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
        "/proc/vmstat",
    ]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "vmstat read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("vmstat should be UTF-8")
}

#[test]
fn vmstat_consumers_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let vmstat = read_vmstat();
    assert!(
        vmstat.lines().any(|line| line == "nr_free_pages 0"),
        "vmstat omitted nr_free_pages:\n{vmstat}"
    );
    for line in vmstat.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        assert_eq!(fields.len(), 2, "malformed vmstat row: {line}");
        assert_eq!(fields[1], "0", "vmstat counter was not zero: {line}");
    }

    let cases = [
        ProgramCase {
            name: "cat vmstat",
            candidates: &["/bin/cat", "/usr/bin/cat"],
            args: &["/proc/vmstat"],
        },
        ProgramCase {
            name: "awk vmstat",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{ print }", "/proc/vmstat"],
        },
        ProgramCase {
            name: "sed vmstat",
            candidates: &["/bin/sed", "/usr/bin/sed"],
            args: &["-n", "p", "/proc/vmstat"],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
