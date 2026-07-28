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

#[test]
fn proc_locks_consumers_are_deterministic_under_strict_verify() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("proc-locks");
    fs::create_dir_all(&build_root).expect("failed to create proc-locks build directory");

    for (name, api) in [("fcntl", "1"), ("lockf", "2"), ("ofd-fcntl", "3")] {
        let guest = build_root.join(name);
        let compile = Command::new("cc")
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(format!("-DLOCK_API={api}"))
            .arg(repository.join("tests/c/proc_locks.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile {name}: {error}"));
        assert!(
            compile.status.success(),
            "failed to compile {name}:\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );

        let strict = Command::new("timeout")
            .args(["--kill-after", "5s", "90s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "run",
                "--backend=ptrace",
                "--strict",
                "--no-virtualize-cpuid",
                "--max-timeslice=disabled",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to run {name}: {error}"));
        let strict_out = String::from_utf8_lossy(&strict.stdout);
        let strict_err = String::from_utf8_lossy(&strict.stderr);
        assert!(
            strict.status.success(),
            "{name} failed strict run\nstdout:\n{strict_out}\nstderr:\n{strict_err}"
        );
        assert!(
            strict_out.contains("proc-locks-virtual-graph-and-aliases-ok"),
            "{name}: content/alias probe omitted its marker\nstdout:\n{strict_out}\nstderr:\n{strict_err}"
        );

        let output = Command::new("timeout")
            .args(["--kill-after", "5s", "90s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log",
                "DEBUG",
                "run",
                "--backend=ptrace",
                "--strict",
                "--verify",
                "--no-virtualize-cpuid",
                "--max-timeslice=disabled",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest)
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
