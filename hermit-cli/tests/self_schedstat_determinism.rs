/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for /proc/<pid>/schedstat: the task's runtime from
//! Detcore's CPU accounting, and no host scheduler counters.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

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
# Each alias names this thread: its runtime, which only grows, then the
# run-queue wait and the run count, which Detcore does not model.
previous = 0
for path in paths:
    with open(path, "rb") as schedstat:
        text = schedstat.read()
    runtime, delay, count = (int(field) for field in text.split())
    assert text.endswith(b"\n") and runtime > 0 and delay == 0 and count == 0, (path, text)
    assert runtime >= previous, (path, runtime, previous)
    previous = runtime
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
        .arg(hermit_test::hermit_binary())
        .args([
            "--log",
            "DEBUG",
            "--backend=ptrace",
            "run",
            "--strict",
            "--proc=standard",
            "--verify",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
            "--",
        ])
        .arg(&program)
        .args(case.args);

    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
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
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args([
        "--log",
        "ERROR",
        "--backend=ptrace",
        "run",
        "--strict",
        "--proc=standard",
        "--panic-on-unsupported-syscalls",
        "--base-env=minimal",
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
        "--",
        "/bin/cat",
        "/proc/self/schedstat",
    ]);
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
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
    // The reader's runtime, the same in every run, and no host counter.
    let schedstat = read_self_schedstat();
    let text = String::from_utf8(schedstat.clone()).unwrap();
    let fields: Vec<u64> = text
        .split_whitespace()
        .map(|field| field.parse().unwrap())
        .collect();
    assert!(
        text.ends_with('\n') && fields.len() == 3 && fields[0] > 0 && fields[1..] == [0, 0],
        "{text:?}"
    );
    assert_eq!(read_self_schedstat(), schedstat);

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
