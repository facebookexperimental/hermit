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
// TODO-HUMAN-REVIEW(PR-927): Review strict verification coverage for PTY counts.
#[test]
fn pty_nr_consumers_verify() {
    assert!(
        Path::new("/proc/sys/kernel/pty/nr").is_file(),
        "/proc/sys/kernel/pty/nr is required"
    );
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("pty-nr");
    fs::create_dir_all(&build_root).expect("failed to create pty-nr build directory");
    let probe = build_root.join("pty-nr-count");
    let compile = Command::new("cc")
        .args(["-O2", "-std=gnu11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/pty_nr_count.c"))
        .arg("-o")
        .arg(&probe)
        .output()
        .expect("failed to compile pty count probe");
    assert!(
        compile.status.success(),
        "pty count probe compilation failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let probe_output = Command::new("timeout")
        .args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--backend=ptrace",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&probe)
        .output()
        .expect("failed to run pty count probe");
    assert!(
        probe_output.status.success()
            && String::from_utf8_lossy(&probe_output.stdout)
                .contains("pty-count-tracks-open-files-ok"),
        "pty count probe failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&probe_output.stdout),
        String::from_utf8_lossy(&probe_output.stderr)
    );
    let cases = [
        ProgramCase {
            name: "cat pty/nr",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &["/proc/sys/kernel/pty/nr"],
        },
        ProgramCase {
            name: "awk pty/nr",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{print $1}", "/proc/sys/kernel/pty/nr"],
        },
        ProgramCase {
            name: "sed pty/nr",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "1p", "/proc/sys/kernel/pty/nr"],
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
