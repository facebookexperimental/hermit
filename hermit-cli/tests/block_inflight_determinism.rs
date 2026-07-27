/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for live block request counts in /sys/block/*/inflight.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: Vec<String>,
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn first_inflight_path() -> Option<PathBuf> {
    let mut paths = fs::read_dir("/sys/block")
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("inflight"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    paths.into_iter().next()
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
        .args(&case.args);

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

fn read_inflight(path: &Path) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "--log",
            "ERROR",
            "run",
            "--backend=ptrace",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
            "/bin/cat",
        ])
        .arg(path);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "block inflight read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("block inflight should be UTF-8")
}

#[test]
fn block_inflight_consumers_are_deterministic_under_strict_verify() {
    let Some(path) = first_inflight_path() else {
        return;
    };
    let _guard = hermit_run_lock();
    let contents = read_inflight(&path);
    assert_eq!(
        contents.split_whitespace().collect::<Vec<_>>(),
        ["0", "0"],
        "block inflight retained host queue depths: {contents:?}"
    );

    let path = path.display().to_string();
    let cases = [
        ProgramCase {
            name: "cat block inflight",
            candidates: &["/bin/cat", "/usr/bin/cat"],
            args: vec![path.clone()],
        },
        ProgramCase {
            name: "awk block inflight",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: vec!["{ print }".to_owned(), path.clone()],
        },
        ProgramCase {
            name: "sed block inflight",
            candidates: &["/bin/sed", "/usr/bin/sed"],
            args: vec!["-n".to_owned(), "p".to_owned(), path],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
