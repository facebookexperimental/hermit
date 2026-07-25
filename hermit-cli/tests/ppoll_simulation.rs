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
fn ppoll_waits_use_nonblocking_probes_and_verify() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ppoll-simulation");
    fs::create_dir_all(&build_root).expect("failed to create ppoll guest build directory");
    let guest = build_root.join("ppoll_simulation");

    let mut compile = Command::new("cc");
    compile
        .args([
            "-O0", "-g", "-pthread", "-std=c11", "-Wall", "-Wextra", "-Werror",
        ])
        .arg(repository.join("tests/c/ppoll_simulation.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "ppoll guest compilation");

    let mut trace_command = Command::new("timeout");
    trace_command
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=trace", "run", "--strict", "--base-env=minimal", "--"])
        .arg(&guest);
    let trace_output = command_output(trace_command, "strict ppoll trace");
    let trace_stdout = String::from_utf8_lossy(&trace_output.stdout);
    let trace_stderr = String::from_utf8_lossy(&trace_output.stderr);
    assert!(
        trace_stdout.contains("ppoll-simulation-ok"),
        "ppoll guest omitted its success marker\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );
    assert!(
        trace_stderr.contains("InternalIOPolling")
            && trace_stderr.contains("Retry #1 for syscall due to result Ok(0)"),
        "ppoll did not use nonblocking scheduler probes\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );

    let mut verify_command = Command::new("timeout");
    verify_command
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--strict",
            "--verify",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest);
    let verify_output = command_output(verify_command, "strict ppoll verification");
    let verify_stdout = String::from_utf8_lossy(&verify_output.stdout);
    let verify_stderr = String::from_utf8_lossy(&verify_output.stderr);
    assert!(
        verify_stdout.contains("Determinism verified")
            || verify_stderr.contains("Determinism verified"),
        "Hermit omitted its determinism marker\nstdout:\n{verify_stdout}\nstderr:\n{verify_stderr}",
    );
}
