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
use std::process::Output;

fn command_output(mut command: Command, label: &str) -> Output {
    hermit_test::configure_guest_execution(&mut command);
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

#[test]
fn zero_copy_pipe_syscalls_fail_closed_by_default_and_allow_compatibility_opt_out() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("zero-copy-pipe-fallback");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    let cases = [
        ("splice", repository.join("tests/c/splice_enosys.c")),
        ("tee", repository.join("tests/c/tee_enosys.c")),
        ("vmsplice", repository.join("tests/c/vmsplice_enosys.c")),
    ];

    for (syscall, source) in cases {
        let guest = build_root.join(format!("{syscall}_enosys"));
        let mut compile = Command::new("cc");
        compile
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(source)
            .arg("-o")
            .arg(&guest);
        command_output(compile, &format!("{syscall} guest compilation"));

        let mut default = Command::new("timeout");
        default
            .args(["--kill-after", "5s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=info",
                "run",
                "--backend=ptrace",
                "--verify",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest);
        let default_output = command_output(
            default,
            &format!("{syscall} default fail-closed verification"),
        );
        let stdout = String::from_utf8_lossy(&default_output.stdout);
        let stderr = String::from_utf8_lossy(&default_output.stderr);
        assert!(
            stdout.contains(&format!("{syscall} deterministically unavailable")),
            "{syscall} default run did not expose deterministic ENOSYS\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{syscall} omitted Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );

        let mut compatibility = Command::new("timeout");
        compatibility
            .args(["--kill-after", "5s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args([
                "--log=off",
                "run",
                "--backend=ptrace",
                "--allow-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest)
            .arg("passthrough");
        let compatibility_output = command_output(
            compatibility,
            &format!("{syscall} opted-in compatibility passthrough"),
        );
        let compatibility_stdout = String::from_utf8_lossy(&compatibility_output.stdout);
        assert_eq!(
            compatibility_stdout.as_ref(),
            format!("{syscall} legacy passthrough preserved\n"),
            "{syscall} compatibility opt-out did not reach the host syscall",
        );
    }
}
