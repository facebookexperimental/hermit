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

fn verify_guest(guest: &Path, label: &str) {
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
        .arg(guest);
    let output = command_output(verify, label);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "{label} omitted Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn meminfo_fields_and_free_use_guest_memory() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("meminfo-determinism");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    for name in ["meminfo_free", "meminfo_available", "meminfo_cached"] {
        let guest = build_root.join(format!("{name}_deterministic"));
        let mut compile = Command::new("cc");
        compile
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join(format!("tests/c/{name}_deterministic.c")))
            .arg("-o")
            .arg(&guest);
        command_output(compile, &format!("{name} guest compilation"));
        verify_guest(&guest, &format!("{name} strict verification"));
    }

    let free = Path::new("/usr/bin/free");
    assert!(
        free.is_file(),
        "free(1) is required by the compatibility corpus"
    );
    verify_guest(free, "free strict verification");
}
