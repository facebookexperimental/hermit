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
fn filesystem_handle_export_refusals_verify() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("name-to-handle-refusal");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    let cases = [
        (
            "regular",
            repository.join("tests/c/name_to_handle_regular_eopnotsupp.c"),
        ),
        (
            "directory",
            repository.join("tests/c/name_to_handle_directory_eopnotsupp.c"),
        ),
        (
            "empty_path",
            repository.join("tests/c/name_to_handle_empty_path_eopnotsupp.c"),
        ),
    ];

    for (mode, source) in cases {
        let guest = build_root.join(format!("name_to_handle_{mode}_eopnotsupp"));
        let mut compile = Command::new("cc");
        compile
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(source)
            .arg("-o")
            .arg(&guest);
        command_output(
            compile,
            &format!("{mode} name_to_handle_at guest compilation"),
        );

        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "90s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=off",
                "run",
                "--backend=ptrace",
                "--strict",
                "--verify",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest);
        let output = command_output(
            verify,
            &format!("{mode} name_to_handle_at strict verification"),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{mode} name_to_handle_at guest omitted Hermit's determinism marker\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}
