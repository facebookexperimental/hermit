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

const RTC_ROOT: &str = "/sys/class/rtc/rtc0";
const RTC_DATE: &str = "/sys/class/rtc/rtc0/date";
const RTC_TIME: &str = "/sys/class/rtc/rtc0/time";
const RTC_EPOCH: &str = "/sys/class/rtc/rtc0/since_epoch";

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: &'static [&'static str],
}

fn required_program(name: &str, candidates: &[&str]) -> PathBuf {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} requires one of {candidates:?}"))
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

fn assert_normalized_attribute(path: &str, expected: &[u8]) {
    let cat = required_program("cat sysfs RTC attribute", &["/usr/bin/cat", "/bin/cat"]);
    let mut command = hermit_command();
    command.arg(cat).arg(path);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));

    assert!(
        output.status.success(),
        "RTC attribute read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(output.stdout, expected, "unexpected normalized {path}");
}

fn assert_l2(case: &ProgramCase) {
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
fn sysfs_rtc_consumers_verify() {
    if ![RTC_DATE, RTC_TIME, RTC_EPOCH]
        .iter()
        .all(|path| Path::new(path).is_file())
    {
        eprintln!("skipping: {RTC_ROOT} does not expose the expected RTC attributes");
        return;
    }

    assert_normalized_attribute(RTC_DATE, b"2021-12-31\n");
    assert_normalized_attribute(RTC_TIME, b"23:59:59\n");
    assert_normalized_attribute(RTC_EPOCH, b"1640995199\n");

    let cases = [
        ProgramCase {
            name: "bash sysfs RTC epoch",
            candidates: &["/usr/bin/bash", "/bin/bash"],
            args: &[
                "-c",
                "for i in {1..100000}; do :; done; cat /sys/class/rtc/rtc0/since_epoch",
            ],
        },
        ProgramCase {
            name: "zsh sysfs RTC time",
            candidates: &["/usr/bin/zsh", "/bin/zsh"],
            args: &[
                "-c",
                "i=0; while [ \"$i\" -lt 5000 ]; do i=$((i+1)); done; cat /sys/class/rtc/rtc0/time",
            ],
        },
        ProgramCase {
            name: "perl sysfs RTC epoch",
            candidates: &["/usr/bin/perl", "/bin/perl"],
            args: &[
                "-e",
                "$x += $_ for 1..5000000; open my $fh, \"<\", \"/sys/class/rtc/rtc0/since_epoch\" or die $!; print while <$fh>",
            ],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
