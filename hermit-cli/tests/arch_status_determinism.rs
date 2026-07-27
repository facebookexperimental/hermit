/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
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
fn arch_status_consumers_are_deterministic_under_strict_verify() {
    let path = Path::new("/proc/self/arch_status");
    let Ok(host_contents) = fs::read_to_string(path) else {
        return;
    };
    if !host_contents.contains("AVX512_elapsed_ms:") {
        return;
    }

    let cat = ProgramCase {
        name: "cat",
        candidates: &["/usr/bin/cat", "/bin/cat"],
        args: &["/proc/self/arch_status"],
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
        .expect("failed to read arch_status");
    assert!(snapshot.status.success());
    let text = String::from_utf8(snapshot.stdout).expect("arch_status should be UTF-8");
    assert!(text.lines().any(|line| line == "AVX512_elapsed_ms:\t0"));

    for case in [
        cat,
        ProgramCase {
            name: "awk",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &[
                "-F",
                "\t",
                "/^AVX512_elapsed_ms:/ { print $2 }",
                "/proc/self/arch_status",
            ],
        },
        ProgramCase {
            name: "sed",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &[
                "-n",
                "s/^AVX512_elapsed_ms:[[:space:]]*//p",
                "/proc/self/arch_status",
            ],
        },
    ] {
        assert_l2(&case);
    }
}
