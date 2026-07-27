/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

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
        .unwrap_or_else(|| panic!("required program {} is missing", case.name))
}

fn assert_l2(case: &ProgramCase) {
    let output = Command::new("timeout")
        .args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log",
            "DEBUG",
            "run",
            "--backend=ptrace",
            "--strict",
            "--verify",
            "--base-env=minimal",
            "--",
        ])
        .arg(required_program(case))
        .args(case.args)
        .output()
        .unwrap_or_else(|error| panic!("failed to verify {}: {error}", case.name));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{} failed strict verification\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name
    );
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "{} omitted verification marker\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name
    );
}

#[test]
fn smaps_rollup_consumers_are_deterministic_under_strict_verify() {
    const HOST_ACCOUNTING_FIELDS: &[&str] = &[
        "Rss",
        "Pss",
        "Pss_Dirty",
        "Pss_Anon",
        "Pss_File",
        "Pss_Shmem",
        "Shared_Clean",
        "Shared_Dirty",
        "Private_Clean",
        "Referenced",
        "KSM",
        "SwapPss",
    ];
    let cat = ProgramCase {
        name: "cat",
        candidates: &["/usr/bin/cat", "/bin/cat"],
        args: &["/proc/self/smaps_rollup"],
    };
    let snapshot = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=ERROR",
            "run",
            "--backend=ptrace",
            "--strict",
            "--base-env=minimal",
            "--",
        ])
        .arg(required_program(&cat))
        .args(cat.args)
        .output()
        .expect("failed to read smaps_rollup");
    assert!(snapshot.status.success());
    let text = String::from_utf8(snapshot.stdout).expect("smaps_rollup should be UTF-8");
    let mut accounting_rows = 0;
    for line in text.lines() {
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let fields = value.split_whitespace().collect::<Vec<_>>();
        if HOST_ACCOUNTING_FIELDS.contains(&label) && fields.len() == 2 && fields[1] == "kB" {
            assert_eq!(fields[0], "0", "smaps accounting was not zeroed: {line}");
            accounting_rows += 1;
        }
    }
    assert!(accounting_rows > 5, "smaps_rollup omitted accounting rows");

    for case in [
        cat,
        ProgramCase {
            name: "sed",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "p", "/proc/self/smaps_rollup"],
        },
        ProgramCase {
            name: "grep",
            candidates: &["/usr/bin/grep", "/bin/grep"],
            args: &["-E", "^(Pss|Pss_File):", "/proc/self/smaps_rollup"],
        },
    ] {
        assert_l2(&case);
    }
}
