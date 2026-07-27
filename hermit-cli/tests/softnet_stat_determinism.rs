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

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: &'static [&'static str],
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
        .args(["--kill-after", "5s", "90s"])
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

#[test]
fn softnet_stat_consumers_are_deterministic_under_strict_verify() {
    assert!(
        Path::new("/proc/net/softnet_stat").is_file(),
        "/proc/net/softnet_stat is required"
    );
    let cases = [
        ProgramCase {
            name: "cat softnet table",
            candidates: &["/bin/cat", "/usr/bin/cat"],
            args: &["/proc/net/softnet_stat"],
        },
        ProgramCase {
            name: "awk virtual softnet CPU row",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &[
                "NF != 15 || $13 != \"00000000\" { exit 1 } END { if (NR != 1) exit 1 }",
                "/proc/net/softnet_stat",
            ],
        },
        ProgramCase {
            name: "sed softnet table",
            candidates: &["/bin/sed", "/usr/bin/sed"],
            args: &["-n", "p", "/proc/net/softnet_stat"],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
