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

        // Plain `--strict` surfaces the guest's stdout, so assert content: the
        // guest holds one WRITE lock, so `/proc/locks` must contain at least one
        // row, and every row's device:inode column must have been rewritten to
        // the synthetic `00:00:<n>` form. A raw host row would carry a nonzero
        // major/minor (for example `08:02:1234567`), so this proves the snapshot
        // is mediated rather than passed through, without depending on a quiet
        // host `/proc/locks`.
        let strict = Command::new("timeout")
            .args(["--kill-after", "5s", "90s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "run",
                "--backend=ptrace",
                "--strict",
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
        let lock_rows: Vec<&str> = strict_out
            .lines()
            .filter(|line| line.split_whitespace().count() >= 7 && line.contains(':'))
            .collect();
        assert!(
            !lock_rows.is_empty(),
            "{name}: guest lock produced no /proc/locks rows\nstdout:\n{strict_out}"
        );
        for row in &lock_rows {
            let object = row
                .split_whitespace()
                .nth_back(2)
                .expect("lock row has an object column");
            assert!(
                object.starts_with("00:00:"),
                "{name}: unsanitized backing object {object:?} in row {row:?}"
            );
        }

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
