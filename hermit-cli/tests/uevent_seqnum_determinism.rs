/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for the host-global kernel uevent sequence number.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
const UEVENT_SEQNUM: &str = "/sys/kernel/uevent_seqnum";

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
            "--backend",
            "ptrace",
            "--strict",
            "--verify",
            "--verify-logs",
            "--panic-on-unsupported-syscalls",
            "--base-env",
            "minimal",
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

#[test]
fn uevent_seqnum_consumers_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    assert!(
        Path::new(UEVENT_SEQNUM).is_file(),
        "{UEVENT_SEQNUM} is required for the portable regression"
    );

    let cases = [
        ProgramCase {
            name: "cat",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &[UEVENT_SEQNUM],
        },
        ProgramCase {
            name: "awk",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{print $1}", UEVENT_SEQNUM],
        },
        ProgramCase {
            name: "sed",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "1p", UEVENT_SEQNUM],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
