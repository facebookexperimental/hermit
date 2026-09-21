/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for host filesystem telemetry in Btrfs commit_stats.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
const LABELS: &[&str] = &[
    "commits",
    "cur_commit_ms",
    "last_commit_ms",
    "max_commit_ms",
    "total_commit_ms",
];

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

fn is_lowercase_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        })
}

fn first_commit_stats_path() -> Option<PathBuf> {
    let mut paths = fs::read_dir("/sys/fs/btrfs")
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_str().is_some_and(is_lowercase_uuid))
        .map(|entry| entry.path().join("commit_stats"))
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

fn read_commit_stats(path: &Path) -> String {
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
        "Btrfs commit_stats read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("Btrfs commit_stats should be UTF-8")
}

#[test]
fn btrfs_commit_stats_consumers_are_deterministic_under_strict_verify() {
    let Some(path) = first_commit_stats_path() else {
        return;
    };
    let _guard = hermit_run_lock();
    let contents = read_commit_stats(&path);
    let rows = contents
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), LABELS.len(), "commit_stats row count changed");
    for (row, expected_label) in rows.iter().zip(LABELS) {
        assert_eq!(
            row,
            &vec![*expected_label, "0"],
            "unexpected commit_stats row"
        );
    }

    let path = path.display().to_string();
    let cases = [
        ProgramCase {
            name: "awk Btrfs commit stats",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: vec!["{ print }".to_owned(), path.clone()],
        },
        ProgramCase {
            name: "sed Btrfs commit stats",
            candidates: &["/bin/sed", "/usr/bin/sed"],
            args: vec!["-n".to_owned(), "p".to_owned(), path.clone()],
        },
        ProgramCase {
            name: "grep Btrfs commit stats",
            candidates: &["/usr/bin/grep", "/bin/grep"],
            args: vec![".".to_owned(), path],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
