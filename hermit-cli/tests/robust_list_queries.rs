/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Output;

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

#[test]
fn robust_list_queries_verify() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("robust-list-queries");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    let cases = [
        ("self", repository.join("tests/c/get_robust_list_self.c")),
        (
            "thread",
            repository.join("tests/c/get_robust_list_thread.c"),
        ),
        ("child", repository.join("tests/c/get_robust_list_child.c")),
    ];

    for (kind, source) in cases {
        let guest = build_root.join(format!("get_robust_list_{kind}"));
        let mut compile = Command::new("cc");
        compile
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(source)
            .args(["-pthread", "-o"])
            .arg(&guest);
        command_output(compile, &format!("{kind} robust-list guest compilation"));

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
            .arg(&guest);
        let output = command_output(verify, &format!("{kind} robust-list strict verification"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{kind} robust-list guest omitted Hermit's determinism marker\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}
