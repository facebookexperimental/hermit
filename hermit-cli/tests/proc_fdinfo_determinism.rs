/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn proc_fdinfo_consumers_are_deterministic_under_strict_verify() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("proc-fdinfo");
    fs::create_dir_all(&build_root).expect("failed to create proc-fdinfo build directory");

    for (name, source) in [("open", "1"), ("openat", "2"), ("memfd", "3")] {
        let guest = build_root.join(name);
        let compile = Command::new("cc")
            .args(["-O2", "-std=gnu11", "-Wall", "-Wextra", "-Werror"])
            .arg(format!("-DFD_SOURCE={source}"))
            .arg(repository.join("tests/c/proc_fdinfo.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile {name}: {error}"));
        assert!(
            compile.status.success(),
            "failed to compile {name}:\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );

        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "5s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log",
                "DEBUG",
                "run",
                "--backend=ptrace",
                "--strict",
                "--verify",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest);
        hermit_test::configure_guest_execution(&mut command);
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("failed to verify {name}: {error}"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{name} failed strict verification\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{name} omitted verification marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}
