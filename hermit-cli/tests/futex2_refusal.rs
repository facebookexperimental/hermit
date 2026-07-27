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

const CASES: &[(&str, &str, &str)] = &[
    (
        "futex_waitv_enosys.c",
        "futex-waitv-enosys-ok",
        "futex_waitv",
    ),
    ("futex_wake_enosys.c", "futex-wake-enosys-ok", "futex_wake"),
    (
        "futex_requeue_enosys.c",
        "futex-requeue-enosys-ok",
        "futex_requeue",
    ),
];

#[test]
fn futex2_feature_probes_receive_deterministic_enosys() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("futex2-refusal");
    fs::create_dir_all(&build_root).expect("failed to create futex2 guest build directory");

    for (source, marker, syscall) in CASES {
        let guest = build_root.join(source.trim_end_matches(".c"));
        let compile = Command::new("cc")
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c").join(source))
            .arg("-o")
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile {source}: {error}"));
        assert!(
            compile.status.success(),
            "failed to compile {source}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&compile.stdout),
            String::from_utf8_lossy(&compile.stderr),
        );

        let trace = Command::new("timeout")
            .args(["--kill-after", "5s", "60s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=trace",
                "run",
                "--backend=ptrace",
                "--strict",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to trace {source}: {error}"));
        assert!(
            trace.status.success(),
            "{source} trace failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            trace.status,
            String::from_utf8_lossy(&trace.stdout),
            String::from_utf8_lossy(&trace.stderr),
        );
        let trace_text = String::from_utf8_lossy(&trace.stdout);
        let trace_stderr = String::from_utf8_lossy(&trace.stderr);
        assert!(
            trace_text.contains(marker),
            "{source} omitted marker {marker}"
        );
        assert!(
            trace_stderr.contains(&format!("inbound syscall: {syscall}(")),
            "{source} trace omitted {syscall}"
        );

        let output = Command::new("timeout")
            .args(["--kill-after", "5s", "60s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=debug",
                "run",
                "--backend=ptrace",
                "--strict",
                "--verify",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to run {source}: {error}"));
        assert!(
            output.status.success(),
            "{source} failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let combined = [output.stdout, output.stderr].concat();
        let text = String::from_utf8_lossy(&combined);
        assert!(
            text.contains("Determinism verified"),
            "{source} omitted determinism marker"
        );
    }
}
