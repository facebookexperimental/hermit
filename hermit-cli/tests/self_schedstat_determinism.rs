/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for host scheduler counters in /proc/self/schedstat.

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

const SCHEDSTAT_ALIAS_CHECK: &str = r#"
import os
import threading

pid = os.getpid()
tid = threading.get_native_id()
paths = [
    "/proc/self/schedstat",
    "/proc/thread-self/schedstat",
    f"/proc/{pid}/schedstat",
    f"/proc/self/task/{tid}/schedstat",
    f"/proc/{pid}/task/{tid}/schedstat",
]
for path in paths:
    with open(path, "rb") as schedstat:
        assert schedstat.read() == b"0 0 0\n", path
"#;

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

fn read_self_schedstat() -> Vec<u8> {
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
        "/proc/self/schedstat",
    ]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "schedstat read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output.stdout
}

#[test]
fn self_schedstat_consumers_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    assert_eq!(read_self_schedstat(), b"0 0 0\n");

    let cases = [
        ProgramCase {
            name: "cat self schedstat",
            candidates: &["/bin/cat", "/usr/bin/cat"],
            args: &["/proc/self/schedstat"],
        },
        ProgramCase {
            name: "awk self schedstat",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{ print $1, $2, $3 }", "/proc/self/schedstat"],
        },
        ProgramCase {
            name: "cut self schedstat",
            candidates: &["/usr/bin/cut", "/bin/cut"],
            args: &["-d", " ", "-f", "1-3", "/proc/self/schedstat"],
        },
        ProgramCase {
            name: "process and thread schedstat aliases",
            candidates: &["/usr/bin/python3", "/bin/python3"],
            args: &["-c", SCHEDSTAT_ALIAS_CHECK],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
