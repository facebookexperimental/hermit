/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for host memory accounting in /proc/self/smaps.

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

fn read_smaps() -> String {
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
        "/proc/self/smaps",
    ]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "smaps read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("smaps should be UTF-8")
}

#[test]
fn smaps_consumers_are_deterministic_under_strict_verify() {
    const ACCOUNTING_FIELDS: &[&str] = &[
        "Rss",
        "Pss",
        "Pss_Dirty",
        "Pss_Anon",
        "Pss_File",
        "Pss_Shmem",
        "Shared_Clean",
        "Shared_Dirty",
        "Private_Clean",
        "Referenced",
        "KSM",
        "SwapPss",
    ];

    let _guard = hermit_run_lock();
    let smaps = read_smaps();
    assert!(!smaps.is_empty(), "smaps should contain mappings");
    assert!(
        smaps.lines().any(|line| line.starts_with("VmFlags:")),
        "smaps omitted mapping flags:\n{smaps}"
    );
    assert!(
        smaps.lines().any(|line| {
            line.strip_prefix("Size:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|size| size > 0)
        }),
        "smaps omitted nonzero mapping sizes:\n{smaps}"
    );
    let mut accounting_rows = 0;
    for line in smaps.lines() {
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        if ACCOUNTING_FIELDS.contains(&label) {
            assert_eq!(
                value.split_whitespace().collect::<Vec<_>>(),
                ["0", "kB"],
                "smaps retained host accounting in {line}"
            );
            accounting_rows += 1;
        }
    }
    assert!(
        accounting_rows > 5,
        "smaps omitted expected accounting rows:\n{smaps}"
    );

    let cases = [
        ProgramCase {
            name: "cat smaps",
            candidates: &["/bin/cat", "/usr/bin/cat"],
            args: &["/proc/self/smaps"],
        },
        ProgramCase {
            name: "awk smaps",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{ print }", "/proc/self/smaps"],
        },
        ProgramCase {
            name: "sed smaps",
            candidates: &["/bin/sed", "/usr/bin/sed"],
            args: &["-n", "p", "/proc/self/smaps"],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
