/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for live counters exposed by /proc/net/protocols.

use std::path::Path;
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

fn read_protocols() -> String {
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
        "/proc/net/protocols",
    ]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "protocol table read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("protocol table should be UTF-8")
}

fn assert_protocol_counters_are_normalized(contents: &str) {
    let mut lines = contents.lines();
    let header = lines
        .next()
        .expect("protocol table should contain a header")
        .split_whitespace()
        .collect::<Vec<_>>();
    assert!(
        header.starts_with(&["protocol", "size", "sockets", "memory"]),
        "unexpected protocol table header: {header:?}"
    );

    let mut saw_accounted_memory = false;
    let mut saw_unaccounted_memory = false;
    for line in lines {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        assert_eq!(
            fields.get(2),
            Some(&"0"),
            "protocol socket count was not normalized: {line}"
        );
        match fields.get(3).copied() {
            Some("0") => saw_accounted_memory = true,
            Some("-1") => saw_unaccounted_memory = true,
            value => panic!("protocol memory count was not normalized: {value:?}: {line}"),
        }
    }
    assert!(
        saw_accounted_memory,
        "protocol table contained no accounted-memory row:\n{contents}"
    );
    assert!(
        saw_unaccounted_memory,
        "protocol table contained no unaccounted-memory row:\n{contents}"
    );
}

#[test]
fn protocol_consumers_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    assert!(
        Path::new("/proc/net/protocols").is_file(),
        "/proc/net/protocols is required for this regression test"
    );
    assert_protocol_counters_are_normalized(&read_protocols());

    let cases = [
        ProgramCase {
            name: "cat protocol table",
            candidates: &["/bin/cat", "/usr/bin/cat"],
            args: &["/proc/net/protocols"],
        },
        ProgramCase {
            name: "awk protocol table",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{ print }", "/proc/net/protocols"],
        },
        ProgramCase {
            name: "sed protocol table",
            candidates: &["/bin/sed", "/usr/bin/sed"],
            args: &["-n", "p", "/proc/net/protocols"],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
