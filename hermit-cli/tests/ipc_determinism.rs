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
use std::sync::OnceLock;

const PATTERNS: [&str; 5] = [
    "pipe-order",
    "pipe-capacity",
    "socketpair",
    "eventfd",
    "epoll",
];
const REPEAT_COUNT: usize = 5;
const TIMEOUT_SECONDS: u64 = 15;

static IPC_GUEST: OnceLock<PathBuf> = OnceLock::new();

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

fn ipc_guest() -> &'static Path {
    IPC_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ipc-determinism");
            fs::create_dir_all(&build_root).expect("failed to create IPC build directory");
            let output = build_root.join("ipc-determinism");
            let mut command = Command::new("cc");
            command
                .args(["-O0", "-g", "-pthread", "-D_GNU_SOURCE"])
                .arg(repository.join("tests/c/ipc_determinism.c"))
                .arg("-o")
                .arg(&output);
            command_output(command, "IPC guest compilation");
            output
        })
        .as_path()
}

fn run_pattern(pattern: &str, iteration: usize) -> String {
    let mut command = Command::new("timeout");
    command
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--",
        ])
        .arg(ipc_guest())
        .arg(pattern);
    let output = command_output(command, &format!("{pattern} iteration {iteration}"));
    String::from_utf8(output.stdout).expect("IPC guest stdout should be UTF-8")
}

#[test]
fn ipc_patterns_are_deterministic_across_five_runs() {
    for pattern in PATTERNS {
        let expected = run_pattern(pattern, 1);
        assert!(
            expected.starts_with(&format!("{pattern}:")),
            "unexpected {pattern} output: {expected:?}"
        );
        for iteration in 2..=REPEAT_COUNT {
            assert_eq!(
                run_pattern(pattern, iteration),
                expected,
                "{pattern} output changed on iteration {iteration}"
            );
        }
    }
}
