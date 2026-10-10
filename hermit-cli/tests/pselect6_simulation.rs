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

#[test]
fn pselect6_preserves_kernel_abi_and_unblocks_scheduler() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("pselect6-simulation");
    fs::create_dir_all(&build_root).expect("failed to create pselect6 guest build directory");
    let guest = build_root.join("pselect6_simulation");

    let mut compile = Command::new("cc");
    compile
        .args([
            "-O0", "-g", "-pthread", "-std=c11", "-Wall", "-Wextra", "-Werror",
        ])
        .arg(repository.join("tests/c/pselect6_simulation.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "pselect6 guest compilation");

    let mut trace_command = Command::new("timeout");
    trace_command
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=trace", "run", "--strict", "--base-env=minimal", "--"])
        .arg(&guest);
    let trace_output = command_output(trace_command, "strict pselect6 trace");
    let trace_stdout = String::from_utf8_lossy(&trace_output.stdout);
    let trace_stderr = String::from_utf8_lossy(&trace_output.stderr);
    assert!(
        trace_stdout.contains("pselect6-simulation-ok"),
        "pselect6 guest omitted its success marker\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );
    assert!(
        trace_stderr
            .lines()
            .any(|line| { line.contains("Retry #1 for syscall due to result Ok(0): pselect6(") }),
        "pselect6 did not retry through deterministic scratch polling\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );

    let mut verify_command = Command::new("timeout");
    verify_command
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--strict",
            "--verify",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest);
    let verify_output = command_output(verify_command, "strict pselect6 verification");
    let verify_stdout = String::from_utf8_lossy(&verify_output.stdout);
    let verify_stderr = String::from_utf8_lossy(&verify_output.stderr);
    assert!(
        verify_stdout.contains("Determinism verified")
            || verify_stderr.contains("Determinism verified"),
        "Hermit omitted its determinism marker\nstdout:\n{verify_stdout}\nstderr:\n{verify_stderr}",
    );
}

/// pselect6 sleeps under its temporary signal mask for the whole call, as
/// Linux installs it (https://github.com/rrnewton/hermit/issues/3991).
///
/// - `blocks`: the timer's SIGALRM, unblocked in the waiter's own mask and
///   blocked by the call's mask, must not end the call. It returned EINTR.
/// - `unblocks`: a SIGALRM blocked in the waiter's own mask and unblocked by
///   the call's mask must end it with EINTR, with the handler run before
///   pselect returns. It returned EINTR, but the handler did not run until the
///   guest unblocked SIGALRM itself.
/// - `unblocks-timed`: the same with a 5 s timeout, whose remaining time the
///   guest prints. The kernel must not write it, measured on the host clock,
///   over the remaining virtual time Hermit writes.
///
/// The guest checks each result against Linux's and exits nonzero on a
/// mismatch. Each mode runs under strict verification.
#[test]
fn pselect6_sleeps_under_its_temporary_mask() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("pselect-temporary-mask");
    fs::create_dir_all(&build_root).expect("failed to create pselect guest build directory");
    let guest = build_root.join("pselect_temporary_mask");
    let mut compile = Command::new("cc");
    compile
        .args(["-O0", "-g", "-pthread", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/pselect_temporary_mask.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "pselect temporary-mask guest compilation");

    for mode in ["blocks", "unblocks", "unblocks-timed"] {
        let verdict = build_root.join(format!("verify-{mode}.json"));
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "5s", "45s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=info",
                "run",
                "--strict",
                "--verify",
                "--verify-strict",
                "--base-env=minimal",
            ])
            .arg(format!("--verify-json={}", verdict.display()))
            .arg("--")
            .arg(&guest)
            .arg(mode);
        let output = command_output(command, &format!("pselect temporary mask {mode}"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&format!("pselect-temporary-mask mode={mode}")),
            "guest omitted its result line\nstdout:\n{stdout}"
        );
        let report: serde_json::Value = serde_json::from_slice(
            &fs::read(&verdict).expect("strict verification wrote no verdict"),
        )
        .expect("strict verification verdict is valid JSON");
        assert_eq!(report["verdict"], serde_json::json!("matched"), "{mode}");
        assert_eq!(report["bitwise_parity"], serde_json::json!(true), "{mode}");
    }
}
