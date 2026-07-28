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
// TODO-HUMAN-REVIEW(PR-933): Review strict verification coverage for AIO counts.
#[test]
fn aio_nr_consumers_verify() {
    assert!(
        Path::new("/proc/sys/fs/aio-nr").is_file(),
        "/proc/sys/fs/aio-nr is required"
    );
    let cases = [
        ProgramCase {
            name: "cat aio-nr",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &["/proc/sys/fs/aio-nr"],
        },
        ProgramCase {
            name: "awk aio-nr",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{print $1}", "/proc/sys/fs/aio-nr"],
        },
        ProgramCase {
            name: "sed aio-nr",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "1p", "/proc/sys/fs/aio-nr"],
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
