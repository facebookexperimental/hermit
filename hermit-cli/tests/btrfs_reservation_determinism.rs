/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

struct ProgramCase<'a> {
    name: &'static str,
    candidates: &'static [&'static str],
    args: Vec<&'a str>,
}

fn required_program(name: &str, candidates: &[&str]) -> PathBuf {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} requires one of {candidates:?}"))
}

fn reservation_path() -> Option<PathBuf> {
    fs::read_dir("/sys/fs/btrfs")
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("allocation/data/bytes_may_use"))
        .find(|path| path.is_file())
}

fn hermit_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "--log",
        "DEBUG",
        "run",
        "--strict",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--",
    ]);
    command
}

fn assert_normalized_reservation(path: &Path) {
    let cat = required_program("cat Btrfs reservation", &["/usr/bin/cat", "/bin/cat"]);
    let mut command = hermit_command();
    command.arg(cat).arg(path);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));

    assert!(
        output.status.success(),
        "Btrfs reservation read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(output.stdout, b"0\n");
}

fn assert_l2(case: &ProgramCase<'_>) {
    let program = required_program(case.name, case.candidates);
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log",
            "DEBUG",
            "run",
            "--strict",
            "--verify",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
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

#[test]
fn btrfs_reservation_consumers_verify() {
    let Some(path) = reservation_path() else {
        eprintln!("skipping: this host does not expose Btrfs allocation reservations");
        return;
    };
    let path = path
        .to_str()
        .expect("Btrfs sysfs reservation path should be UTF-8");

    assert_normalized_reservation(Path::new(path));
    let cases = [
        ProgramCase {
            name: "cat Btrfs reservation",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: vec![path],
        },
        ProgramCase {
            name: "awk Btrfs reservation",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: vec!["{print $1}", path],
        },
        ProgramCase {
            name: "sed Btrfs reservation",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: vec!["-n", "1p", path],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
