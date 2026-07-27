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

const AVG_FREQ_PATH: &str = "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_avg_freq";

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
fn cpufreq_average_consumers_verify() {
    if !Path::new(AVG_FREQ_PATH).is_file() {
        eprintln!("skipping: {AVG_FREQ_PATH} is not exposed by this host's cpufreq driver");
        return;
    }

    let cases = [
        ProgramCase {
            name: "cat average CPU frequency",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &[AVG_FREQ_PATH],
        },
        ProgramCase {
            name: "awk average CPU frequency",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{print $1}", AVG_FREQ_PATH],
        },
        ProgramCase {
            name: "sed average CPU frequency",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "1p", AVG_FREQ_PATH],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
