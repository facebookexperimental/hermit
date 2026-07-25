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
fn madvise_policy_verifies_in_run_record_and_kvm_modes() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("madvise-determinism");
    fs::create_dir_all(&build_root).expect("failed to create madvise guest build directory");
    let guest = build_root.join("madvise_determinism");

    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/madvise_determinism.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "madvise guest compilation");

    for (label, extra_arg) in [
        ("strict madvise verification", None),
        ("passthru-opt madvise verification", Some("--passthru-opt")),
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
        if let Some(arg) = extra_arg {
            verify.arg(arg);
        }
        verify.arg("--").arg(&guest);
        let output = command_output(verify, label);
        assert_marker(&output, "Determinism verified", label);
    }

    if Path::new("/dev/kvm").exists() {
        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "30s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=off",
                "--backend=kvm",
                "run",
                "--strict",
                "--verify",
                "--preemption-timeout=disabled",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest)
            .arg("--kvm");
        let output = command_output(verify, "KVM madvise verification");
        assert_marker(&output, "Determinism verified", "KVM madvise verification");
    }

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
        .arg(&guest)
        .arg("--record");
    let output = command_output(record, "madvise record/replay verification");
    assert_marker(
        &output,
        "Success: replay matched recording",
        "madvise record/replay verification",
    );
}
