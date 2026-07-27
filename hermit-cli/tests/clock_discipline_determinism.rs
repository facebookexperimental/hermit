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
        "adjtimex_deterministic.c",
        "adjtimex-ok state=5 status=64 tick=10000",
        "adjtimex",
    ),
    (
        "clock_adjtime_deterministic.c",
        "clock-adjtime-ok state=5 status=64 tick=10000",
        "clock_adjtime",
    ),
    ("syslog_deterministic.c", "syslog-ok size=0", "syslog"),
];

#[test]
fn clock_discipline_and_kernel_log_are_host_independent() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("clock-discipline-determinism");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

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
        assert!(
            String::from_utf8_lossy(&trace.stdout).contains(marker),
            "{source} omitted marker {marker}"
        );
        assert!(
            String::from_utf8_lossy(&trace.stderr)
                .contains(&format!("inbound syscall: {syscall}(")),
            "{source} trace omitted {syscall}"
        );

        let verify = Command::new("timeout")
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
            .unwrap_or_else(|error| panic!("failed to verify {source}: {error}"));
        assert!(
            verify.status.success(),
            "{source} verification failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            verify.status,
            String::from_utf8_lossy(&verify.stdout),
            String::from_utf8_lossy(&verify.stderr),
        );
        let combined = [verify.stdout, verify.stderr].concat();
        assert!(
            String::from_utf8_lossy(&combined).contains("Determinism verified"),
            "{source} omitted determinism marker"
        );
    }
}
