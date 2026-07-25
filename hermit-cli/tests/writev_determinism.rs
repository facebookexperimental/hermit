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
fn writev_uses_fd_aware_scheduling_and_verifies() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("writev-determinism");
    fs::create_dir_all(&build_root).expect("failed to create writev guest build directory");
    let guest = build_root.join("writev_determinism");

    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/writev_determinism.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "writev guest compilation");

    let mut trace = Command::new("timeout");
    trace
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=trace",
            "run",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest);
    let trace_output = command_output(trace, "strict writev trace");
    let trace_stdout = String::from_utf8_lossy(&trace_output.stdout);
    let trace_stderr = String::from_utf8_lossy(&trace_output.stderr);
    assert!(
        trace_stdout.contains("writev-determinism-ok"),
        "writev guest omitted its success marker\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );
    assert!(
        trace_stderr.contains("inbound syscall: writev")
            && trace_stderr.contains(
                "NonblockableSyscall: converting to nonblocking syscall (internal polling): writev",
            )
            && trace_stderr.contains("Retry #1 for atomic blocking pipe writev after Err(EAGAIN)"),
        "writev did not reach typed dispatch and internal-fd scheduling\n\
         stdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );

    for (label, extra_arg) in [
        ("strict writev verification", None),
        ("passthru-opt writev verification", Some("--passthru-opt")),
    ] {
        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "30s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=off",
                "run",
                "--strict",
                "--verify",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
            ]);
        if let Some(arg) = extra_arg {
            verify.arg(arg);
        }
        verify.arg("--").arg(&guest);
        let verify_output = command_output(verify, label);
        let verify_stdout = String::from_utf8_lossy(&verify_output.stdout);
        let verify_stderr = String::from_utf8_lossy(&verify_output.stderr);
        assert!(
            verify_stdout.contains("Determinism verified")
                || verify_stderr.contains("Determinism verified"),
            "Hermit omitted its determinism marker for {label}\n\
             stdout:\n{verify_stdout}\nstderr:\n{verify_stderr}",
        );
    }

    // Exercise record mode on pipe retries separately. Replaying dynamically allocated
    // pipe fds is currently blocked by the recorder's independent fd-numbering gap.
    let pipe_recording = build_root.join("pipe-recording");
    let _ = fs::remove_dir_all(&pipe_recording);
    let mut pipe_record = Command::new("timeout");
    pipe_record
        .args(["--kill-after", "5s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=30"])
        .arg("--data-dir")
        .arg(&pipe_recording)
        .arg("--")
        .arg(&guest)
        .arg("record-pipe");
    let pipe_record_output = command_output(pipe_record, "writev pipe recording");
    let pipe_record_stdout = String::from_utf8_lossy(&pipe_record_output.stdout);
    let pipe_record_stderr = String::from_utf8_lossy(&pipe_record_output.stderr);
    assert!(
        pipe_record_stdout.contains("writev-determinism-ok")
            || pipe_record_stderr.contains("writev-determinism-ok"),
        "recorded writev pipe workload omitted its success marker\n\
         stdout:\n{pipe_record_stdout}\nstderr:\n{pipe_record_stderr}",
    );

    let recording = build_root.join("recording");
    let _ = fs::remove_dir_all(&recording);
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "5s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "record",
            "start",
            "--verify",
            "--record-timeout=30",
        ])
        .arg("--data-dir")
        .arg(&recording)
        .arg("--")
        .arg(&guest)
        .arg("record");
    let record_output = command_output(record, "writev record/replay verification");
    let record_stdout = String::from_utf8_lossy(&record_output.stdout);
    let record_stderr = String::from_utf8_lossy(&record_output.stderr);
    assert!(
        record_stdout.contains("Success: replay matched recording")
            || record_stderr.contains("Success: replay matched recording"),
        "Hermit omitted its replay-match marker\n\
         stdout:\n{record_stdout}\nstderr:\n{record_stderr}",
    );
}
