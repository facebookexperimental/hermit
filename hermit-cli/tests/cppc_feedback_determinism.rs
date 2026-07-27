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

const FEEDBACK_COUNTERS: &str = "/sys/devices/system/cpu/cpu0/acpi_cppc/feedback_ctrs";

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
        .unwrap_or_else(|| panic!("{} requires one of {:?}", case.name, case.candidates))
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

fn assert_normalized_feedback() {
    let mut command = hermit_command();
    command.args(["/usr/bin/cat", FEEDBACK_COUNTERS]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));

    assert!(
        output.status.success(),
        "feedback read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(output.stdout, b"ref:0 del:0\n");
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
            "--strict",
            "--verify",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--",
        ])
        .arg(program)
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
fn cppc_feedback_consumers_verify() {
    if !Path::new(FEEDBACK_COUNTERS).is_file() {
        eprintln!("skipping: this host does not expose ACPI CPPC feedback counters");
        return;
    }

    assert_normalized_feedback();
    let cases = [
        ProgramCase {
            name: "cat CPPC feedback counters",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &[FEEDBACK_COUNTERS],
        },
        ProgramCase {
            name: "awk CPPC reference counter",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{print $1}", FEEDBACK_COUNTERS],
        },
        ProgramCase {
            name: "sed CPPC feedback counters",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "1p", FEEDBACK_COUNTERS],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
