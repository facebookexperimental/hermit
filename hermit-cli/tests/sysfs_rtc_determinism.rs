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

fn hermit_command(epoch: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "--log",
        "DEBUG",
        "run",
        "--strict",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    if let Some(epoch) = epoch {
        command.arg(format!("--epoch={epoch}"));
    }
    command.arg("--");
    command
}

fn read_normalized_attribute(path: &str, epoch: Option<&str>) -> Vec<u8> {
    let cat = required_program("cat sysfs RTC attribute", &["/usr/bin/cat", "/bin/cat"]);
    let mut command = hermit_command(epoch);
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
    output.stdout
}

fn assert_normalized_attribute(path: &str, expected: &[u8]) {
    assert_eq!(
        read_normalized_attribute(path, None),
        expected,
        "unexpected normalized {path}"
    );
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

    assert_normalized_attribute(RTC_DATE, b"2026-01-01\n");
    let rtc_time = String::from_utf8(read_normalized_attribute(RTC_TIME, None))
        .expect("RTC time should be UTF-8");
    assert!(
        rtc_time.starts_with("00:00:"),
        "unexpected default RTC time: {rtc_time:?}"
    );
    let rtc_epoch = String::from_utf8(read_normalized_attribute(RTC_EPOCH, None))
        .expect("RTC epoch should be UTF-8")
        .trim()
        .parse::<i64>()
        .expect("RTC epoch should be an integer");
    assert!(
        (1_767_225_600..1_767_225_660).contains(&rtc_epoch),
        "unexpected default RTC epoch: {rtc_epoch}"
    );

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

#[test]
fn sysfs_rtc_tracks_custom_epoch_and_virtual_time() {
    if ![RTC_DATE, RTC_TIME, RTC_EPOCH]
        .iter()
        .all(|path| Path::new(path).is_file())
    {
        eprintln!("skipping: {RTC_ROOT} does not expose the expected RTC attributes");
        return;
    }

    let epoch = "2000-01-01T12:34:56+00:00";
    assert_eq!(
        read_normalized_attribute(RTC_DATE, Some(epoch)),
        b"2000-01-01\n"
    );
    let rtc_time = String::from_utf8(read_normalized_attribute(RTC_TIME, Some(epoch)))
        .expect("RTC time should be UTF-8");
    assert!(
        rtc_time.starts_with("12:34:"),
        "unexpected custom RTC time: {rtc_time:?}"
    );
    let rtc_epoch = String::from_utf8(read_normalized_attribute(RTC_EPOCH, Some(epoch)))
        .expect("RTC epoch should be UTF-8")
        .trim()
        .parse::<i64>()
        .expect("RTC epoch should be an integer");
    assert!(
        (946_730_096..946_730_156).contains(&rtc_epoch),
        "unexpected custom RTC epoch: {rtc_epoch}"
    );

    let python = required_program("Python sysfs RTC clock probe", &["/usr/bin/python3"]);
    let midnight_epoch = "2000-12-31T23:59:59+00:00";
    let mut command = hermit_command(Some(midnight_epoch));
    command.arg(python).args([
        "-c",
        "import time; time.sleep(2); print(open('/sys/class/rtc/rtc0/date').read().strip()); print(open('/sys/class/rtc/rtc0/time').read().strip()); print(open('/sys/class/rtc/rtc0/since_epoch').read().strip())",
    ]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "custom-epoch RTC probe failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let output = String::from_utf8(output.stdout).expect("RTC attributes should be UTF-8");
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some("2001-01-01"));
    assert_ne!(lines.next(), Some("23:59:59"));
    let advanced_epoch = lines
        .next()
        .expect("RTC output omitted since_epoch")
        .parse::<i64>()
        .expect("since_epoch should be an integer");
    assert!(
        advanced_epoch >= 978_307_201,
        "RTC did not advance by the two-second virtual sleep: {output}"
    );
    assert_eq!(lines.next(), None, "unexpected RTC output: {output}");
}
