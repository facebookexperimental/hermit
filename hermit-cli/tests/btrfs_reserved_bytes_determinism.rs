/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for live Btrfs block reservations.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: Vec<String>,
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

fn first_reserved_bytes_path() -> Option<PathBuf> {
    let mut paths = fs::read_dir("/sys/fs/btrfs")
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_str().is_some_and(is_lowercase_uuid))
        .map(|entry| entry.path().join("allocation/data/bytes_reserved"))
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
        .arg(program)
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

fn read_reserved_bytes(path: &Path) -> Vec<u8> {
    let case = ProgramCase {
        name: "cat Btrfs reserved bytes",
        candidates: &["/usr/bin/cat", "/bin/cat"],
        args: Vec::new(),
    };
    let program = required_program(&case);
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
        ])
        .arg(program)
        .arg(path);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "Btrfs bytes_reserved read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output.stdout
}

#[test]
fn btrfs_reserved_bytes_consumers_are_deterministic_under_strict_verify() {
    let Some(path) = first_reserved_bytes_path() else {
        eprintln!("skipping: this host does not expose Btrfs block reservations");
        return;
    };

    assert_eq!(read_reserved_bytes(&path), b"0\n");

    let path = path.display().to_string();
    let cases = [
        ProgramCase {
            name: "cat Btrfs reserved bytes",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: vec![path.clone()],
        },
        ProgramCase {
            name: "awk Btrfs reserved bytes",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: vec!["{print $1}".to_owned(), path.clone()],
        },
        ProgramCase {
            name: "sed Btrfs reserved bytes",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: vec!["-n".to_owned(), "1p".to_owned(), path],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
