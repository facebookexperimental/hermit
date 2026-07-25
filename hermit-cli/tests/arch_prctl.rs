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

fn assert_marker(output: &Output, marker: &str, label: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains(marker) || stderr.contains(marker),
        "{label} omitted {marker:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
}

#[test]
fn arch_prctl_controls_verify_in_run_and_record_modes() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("arch-prctl-determinism");
    fs::create_dir_all(&build_root).expect("failed to create arch_prctl guest build directory");
    let guest = build_root.join("arch_prctl_determinism");

    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/arch_prctl_determinism.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "arch_prctl guest compilation");

    for (label, extra_args) in [
        ("strict arch_prctl verification", None),
        (
            "passthru-opt arch_prctl verification",
            Some("--passthru-opt"),
        ),
    ] {
        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "30s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=off",
                "run",
                "--strict",
                "--verify",
                "--preemption-timeout=disabled",
                "--base-env=minimal",
            ]);
        if let Some(arg) = extra_args {
            verify.arg(arg);
        }
        verify.arg("--").arg(&guest);
        let output = command_output(verify, label);
        assert_marker(&output, "Determinism verified", label);
    }

    let mut host_cpuid = Command::new("timeout");
    host_cpuid
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--strict",
            "--verify",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .arg("--host-cpuid");
    let output = command_output(host_cpuid, "host CPUID passthrough verification");
    assert_marker(
        &output,
        "Determinism verified",
        "host CPUID passthrough verification",
    );

    let recording = build_root.join("recording");
    let _ = fs::remove_dir_all(&recording);
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "5s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "record",
            "start",
            "--verify",
            "--record-timeout",
            "30",
        ])
        .arg("--data-dir")
        .arg(&recording)
        .arg("--")
        .arg(&guest);
    let output = command_output(record, "arch_prctl record/replay verification");
    assert_marker(
        &output,
        "Success: replay matched recording",
        "arch_prctl record/replay verification",
    );
}
