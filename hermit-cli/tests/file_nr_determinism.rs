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

const POSITIONAL_READ_CHECK: &str = r#"
import os

expected = b"0\t0\t9223372036854775807\n"
fd = os.open("/proc/sys/fs/file-nr", os.O_RDONLY)
assert os.read(fd, 4) == expected[:4]
assert os.lseek(fd, 0, os.SEEK_SET) == 0
assert os.read(fd, 128) == expected
assert os.pread(fd, 128, 0) == expected
os.close(fd)

fd = os.open("/proc/sys/fs/file-nr", os.O_RDONLY)
assert os.pread(fd, 128, 0) == expected
os.close(fd)

with open("/proc/sys/fs/file-max", "rb") as file_max:
    assert file_max.read() == expected.split(b"\t")[2]
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

#[test]
fn file_nr_consumers_verify() {
    assert!(
        Path::new("/proc/sys/fs/file-nr").is_file(),
        "/proc/sys/fs/file-nr is required"
    );
    assert!(
        Path::new("/proc/sys/fs/file-max").is_file(),
        "/proc/sys/fs/file-max is required"
    );
    let cases = [
        ProgramCase {
            name: "cat file-nr",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &["/proc/sys/fs/file-nr"],
        },
        ProgramCase {
            name: "awk file-nr allocation and maximum",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{print $1,$3}", "/proc/sys/fs/file-nr"],
        },
        ProgramCase {
            name: "cut file-nr allocation",
            candidates: &["/usr/bin/cut", "/bin/cut"],
            args: &["-f1", "/proc/sys/fs/file-nr"],
        },
        ProgramCase {
            name: "file-nr positional reads and paired maximum",
            candidates: &["/usr/bin/python3", "/bin/python3"],
            args: &["-c", POSITIONAL_READ_CHECK],
        },
    ];

    for case in &cases {
        let program = find_program(case);
        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "90s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=off",
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
