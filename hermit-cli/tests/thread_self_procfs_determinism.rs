/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for the per-thread procfs identity aliases.

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
            "--backend",
            "ptrace",
            "--strict",
            "--verify",
            "--verify-logs",
            "--panic-on-unsupported-syscalls",
            "--base-env",
            "minimal",
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
        "{} failed ptrace L2 strict verification ({rendered})\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
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
fn thread_self_stat_consumers_are_deterministic_under_strict_verify() {
    assert!(
        Path::new("/proc/thread-self/stat").is_file(),
        "/proc/thread-self/stat is required"
    );

    let cases = [
        ProgramCase {
            name: "cat thread stat",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &["/proc/thread-self/stat"],
        },
        ProgramCase {
            name: "awk thread counters",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{print $1, $10, $22, $39}", "/proc/thread-self/stat"],
        },
        ProgramCase {
            name: "sed thread stat",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "1p", "/proc/thread-self/stat"],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}

#[test]
fn thread_self_fd_keeps_the_opener_identity() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("thread-self-procfs-handoff");
    fs::create_dir_all(&build_root).expect("failed to create thread-self build directory");
    let guest = build_root.join("thread-self-procfs-handoff");
    let compile = Command::new("cc")
        .args([
            "-O2",
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-pthread",
        ])
        .arg(repository.join("tests/c/thread_self_procfs_handoff.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to start C compiler");
    assert!(
        compile.status.success(),
        "failed to compile thread-self handoff guest:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let output = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log",
            "DEBUG",
            "run",
            "--backend",
            "ptrace",
            "--strict",
            "--verify",
            "--verify-logs",
            "--panic-on-unsupported-syscalls",
            "--base-env",
            "minimal",
            "--",
        ])
        .arg(&guest)
        .output()
        .expect("failed to start Hermit");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "thread-self handoff failed ptrace L2 strict verification\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "thread-self handoff omitted Hermit's verification marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
