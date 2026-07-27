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
fn socket_timestamp_ioctls_use_logical_time() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("socket-ioctl-timestamp");
    std::fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let guest = build_root.join("socket-ioctl-timestamp");

    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/socket_ioctl_timestamp.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile socket ioctl timestamp guest");
    assert!(
        compile.status.success(),
        "guest compilation failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    for backend in ["ptrace", "dbi", "liteinst"] {
        for mode in ["v4-us", "v4-ns", "v6-us"] {
            let verify = Command::new("timeout")
                .args(["--kill-after", "5s", "90s"])
                .arg(env!("CARGO_BIN_EXE_hermit"))
                .args(["--log=off", "run"])
                .arg(format!("--backend={backend}"))
                .args(["--strict", "--verify", "--base-env=minimal", "--"])
                .arg(&guest)
                .arg(mode)
                .output()
                .unwrap_or_else(|error| panic!("failed to run {backend}/{mode}: {error}"));
            let stdout = String::from_utf8_lossy(&verify.stdout);
            let stderr = String::from_utf8_lossy(&verify.stderr);
            assert!(
                verify.status.success(),
                "{backend}/{mode} strict verification failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                verify.status
            );
            assert!(
                stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
                "{backend}/{mode} omitted Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
        }
    }

    let realtime = Command::new("timeout")
        .args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--backend=ptrace",
            "--no-virtualize-time",
            "--no-virtualize-metadata",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .arg("v4-us")
        .output()
        .expect("failed to run host-clock socket timestamp case");
    assert!(
        realtime.status.success(),
        "host-clock socket timestamp case failed: {}\nstdout:\n{}\nstderr:\n{}",
        realtime.status,
        String::from_utf8_lossy(&realtime.stdout),
        String::from_utf8_lossy(&realtime.stderr)
    );
    let seconds: i64 = String::from_utf8_lossy(&realtime.stdout)
        .trim()
        .split_once('.')
        .expect("guest should print a timeval")
        .0
        .parse()
        .expect("guest should print numeric timeval seconds");
    assert!(
        seconds >= 1_704_067_200,
        "--no-virtualize-time returned the fixed logical epoch: {seconds}"
    );
}
