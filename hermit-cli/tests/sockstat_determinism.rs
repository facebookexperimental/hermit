/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for host-global counters exposed by /proc/net/sockstat.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: &'static [&'static str],
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            "--strict",
            "--verify",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
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

fn read_sockstat() -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "--log",
        "ERROR",
        "run",
        "--strict",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--",
        "/bin/cat",
        "/proc/net/sockstat",
    ]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "sockstat read failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("sockstat should be UTF-8")
}

fn field_value(contents: &str, line_prefix: &str, field: &str) -> u64 {
    let fields = contents
        .lines()
        .find(|line| line.starts_with(line_prefix))
        .unwrap_or_else(|| panic!("sockstat omitted {line_prefix}:\n{contents}"))
        .split_whitespace()
        .collect::<Vec<_>>();
    let field_index = fields
        .iter()
        .position(|value| *value == field)
        .unwrap_or_else(|| panic!("sockstat {line_prefix} omitted {field}:\n{contents}"));
    fields
        .get(field_index + 1)
        .unwrap_or_else(|| panic!("sockstat {line_prefix} omitted {field}'s value:\n{contents}"))
        .parse()
        .unwrap_or_else(|error| {
            panic!("sockstat {line_prefix} {field} is not numeric: {error}:\n{contents}")
        })
}

#[test]
fn sockstat_consumers_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let sockstat = read_sockstat();
    assert_eq!(
        field_value(&sockstat, "TCP:", "alloc"),
        field_value(&sockstat, "TCP:", "inuse"),
        "TCP alloc should expose only guest-visible in-use sockets:\n{sockstat}"
    );
    assert_eq!(
        field_value(&sockstat, "TCP:", "orphan"),
        0,
        "TCP orphan should not expose the host-global counter:\n{sockstat}"
    );
    assert_eq!(
        field_value(&sockstat, "TCP:", "mem"),
        0,
        "TCP mem should not expose host-global page accounting:\n{sockstat}"
    );
    assert_eq!(
        field_value(&sockstat, "UDP:", "mem"),
        0,
        "UDP mem should not expose host-global page accounting:\n{sockstat}"
    );

    let cases = [
        ProgramCase {
            name: "ss summary",
            candidates: &["/usr/sbin/ss", "/usr/bin/ss"],
            args: &["-s"],
        },
        ProgramCase {
            name: "awk TCP allocation counter",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["/^TCP:/ {print $9}", "/proc/net/sockstat"],
        },
        ProgramCase {
            name: "sed TCP memory counter",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "s/^TCP:.* mem //p", "/proc/net/sockstat"],
        },
    ];

    for case in &cases {
        assert!(
            Path::new("/proc/net/sockstat").is_file(),
            "/proc/net/sockstat is required for {}",
            case.name
        );
        assert_l2(case);
    }
}
