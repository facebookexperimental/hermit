/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::process::Command;

#[test]
fn socket_receive_timestamps_use_logical_time() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("socket-timestamp-determinism");
    std::fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    for name in ["socket_timestamp_timeval", "socket_timestamp_timespec"] {
        let source = repository.join(format!("tests/c/{name}.c"));
        let guest = build_root.join(name);
        let compile = Command::new("cc")
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(source)
            .arg("-o")
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile {name}: {error}"));
        assert!(
            compile.status.success(),
            "{name} compilation failed: {}",
            String::from_utf8_lossy(&compile.stderr)
        );

        let verify = Command::new("timeout")
            .args(["--kill-after", "5s", "90s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=off",
                "run",
                "--backend=ptrace",
                "--strict",
                "--verify",
                "--base-env=minimal",
                "--",
            ])
            .arg(guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to run {name}: {error}"));
        let stdout = String::from_utf8_lossy(&verify.stdout);
        let stderr = String::from_utf8_lossy(&verify.stderr);
        assert!(
            verify.status.success(),
            "{name} strict verification failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            verify.status
        );
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{name} omitted Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}
