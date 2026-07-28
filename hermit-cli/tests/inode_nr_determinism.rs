/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: &'static [&'static str],
}

const INODE_RELATION_CHECK: &str = r#"
import os

inode_nr = b"0\t0\n"
inode_state = b"0\t0\t0\t0\t0\t0\t0\n"
fd = os.open("/proc/sys/fs/inode-nr", os.O_RDONLY)
assert os.pread(fd, 128, 0) == inode_nr
assert os.read(fd, 1) == inode_nr[:1]
assert os.lseek(fd, 0, os.SEEK_SET) == 0
assert os.read(fd, 128) == inode_nr
os.close(fd)

with open("/proc/sys/fs/inode-state", "rb") as state:
    observed = state.read()
assert observed == inode_state
assert observed.split()[:2] == inode_nr.split()
"#;

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn find_program(case: &ProgramCase) -> PathBuf {
    case.candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{} requires one of {:?}", case.name, case.candidates))
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-914): Review strict verification coverage for inode counters.
#[test]
fn inode_nr_consumers_verify() {
    assert!(
        Path::new("/proc/sys/fs/inode-nr").is_file(),
        "/proc/sys/fs/inode-nr is required"
    );
    assert!(
        Path::new("/proc/sys/fs/inode-state").is_file(),
        "/proc/sys/fs/inode-state is required"
    );
    let cases = [
        ProgramCase {
            name: "cat inode-nr",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &["/proc/sys/fs/inode-nr"],
        },
        ProgramCase {
            name: "awk inode-nr allocation and free counts",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{print $1,$2}", "/proc/sys/fs/inode-nr"],
        },
        ProgramCase {
            name: "cut inode-nr allocation count",
            candidates: &["/usr/bin/cut", "/bin/cut"],
            args: &["-f1", "/proc/sys/fs/inode-nr"],
        },
        ProgramCase {
            name: "inode positional reads and paired state",
            candidates: &["/usr/bin/python3", "/bin/python3"],
            args: &["-c", INODE_RELATION_CHECK],
        },
    ];

    for case in &cases {
        let program = find_program(case);
        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "90s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=info",
                "run",
                "--backend=ptrace",
                "--strict",
                "--verify",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(program)
            .args(case.args);
        let output = command_output(verify, case.name);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{} omitted Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}",
            case.name
        );
    }
}
