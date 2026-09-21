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
fn ppoll_readonly_zero_timeout_preserves_ready_result() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ppoll-readonly-zero-timeout");
    fs::create_dir_all(&build_root).expect("failed to create ppoll guest build directory");
    let guest = build_root.join("ppoll_simulation");

    let mut compile = Command::new("cc");
    compile
        .args([
            "-O0", "-g", "-pthread", "-std=c11", "-Wall", "-Wextra", "-Werror",
        ])
        .arg(repository.join("tests/c/ppoll_simulation.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "ppoll zero-timeout guest compilation");

    let mut native = Command::new("timeout");
    native
        .args(["--kill-after", "5s", "30s"])
        .arg(&guest)
        .arg("masked-readonly-zero-timeout");
    let native_output = command_output(native, "native masked read-only zero-timeout ppoll");
    let native_stdout = String::from_utf8_lossy(&native_output.stdout);
    let native_stderr = String::from_utf8_lossy(&native_output.stderr);
    assert!(
        native_stdout.contains("ppoll-simulation-ok"),
        "native masked read-only zero-timeout ppoll omitted its success marker\nstdout:\n{native_stdout}\nstderr:\n{native_stderr}",
    );

    let verdict_directory =
        tempfile::tempdir().expect("failed to create zero-timeout verdict directory");
    let verdict = verdict_directory.path().join("verify.json");
    let mut verify = Command::new("timeout");
    verify
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "--backend=ptrace",
            "run",
            "--strict",
            "--verify",
            "--verify-strict",
        ])
        .arg(format!("--verify-json={}", verdict.display()))
        .args(["--base-env=minimal", "--"])
        .arg(&guest)
        .arg("masked-readonly-zero-timeout");
    let verify_output = command_output(
        verify,
        "strict ptrace masked read-only zero-timeout ppoll verification",
    );
    let verify_stdout = String::from_utf8_lossy(&verify_output.stdout);
    let verify_stderr = String::from_utf8_lossy(&verify_output.stderr);
    assert!(
        verify_stdout.contains("Determinism verified")
            || verify_stderr.contains("Determinism verified"),
        "Hermit omitted its zero-timeout ppoll determinism marker\nstdout:\n{verify_stdout}\nstderr:\n{verify_stderr}",
    );
    let verdict = fs::read_to_string(&verdict).expect("failed to read zero-timeout ppoll verdict");
    assert!(
        verdict.contains("\"bitwise_parity\":true"),
        "masked read-only zero-timeout ppoll did not meet canonical INFO parity: {verdict}",
    );
}

#[test]
fn ppoll_nonsequentialized_kernel_wait_checks_mask_access() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("ppoll-inaccessible-kernel-wait-mask");
    fs::create_dir_all(&build_root).expect("failed to create ppoll guest build directory");
    let guest = build_root.join("ppoll_simulation");

    let mut compile = Command::new("cc");
    compile
        .args([
            "-O0", "-g", "-pthread", "-std=c11", "-Wall", "-Wextra", "-Werror",
        ])
        .arg(repository.join("tests/c/ppoll_simulation.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "ppoll inaccessible-mask guest compilation");

    let mut native = Command::new("timeout");
    native
        .args(["--kill-after", "5s", "30s"])
        .arg(&guest)
        .arg("inaccessible-mask-kernel-wait");
    let native_output = command_output(native, "native inaccessible-mask ppoll");
    let native_stdout = String::from_utf8_lossy(&native_output.stdout);
    let native_stderr = String::from_utf8_lossy(&native_output.stderr);
    assert!(
        native_stdout.contains("ppoll-simulation-ok"),
        "native inaccessible-mask ppoll omitted its success marker\nstdout:\n{native_stdout}\nstderr:\n{native_stderr}",
    );

    let verdict_directory =
        tempfile::tempdir().expect("failed to create inaccessible-mask verdict directory");
    let verdict = verdict_directory.path().join("verify.json");
    let mut verify = Command::new("timeout");
    verify
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "--backend=ptrace",
            "run",
            "--no-sequentialize-threads",
            "--verify",
            "--verify-strict",
        ])
        .arg(format!("--verify-json={}", verdict.display()))
        .args(["--base-env=minimal", "--"])
        .arg(&guest)
        .arg("inaccessible-mask-kernel-wait");
    let verify_output = command_output(
        verify,
        "nonsequentialized ptrace inaccessible-mask ppoll verification",
    );
    let verify_stdout = String::from_utf8_lossy(&verify_output.stdout);
    let verify_stderr = String::from_utf8_lossy(&verify_output.stderr);
    assert!(
        verify_stdout.contains("Determinism verified")
            || verify_stderr.contains("Determinism verified"),
        "Hermit omitted its inaccessible-mask ppoll determinism marker\nstdout:\n{verify_stdout}\nstderr:\n{verify_stderr}",
    );
    let verdict =
        fs::read_to_string(&verdict).expect("failed to read inaccessible-mask ppoll verdict");
    assert!(
        verdict.contains("\"bitwise_parity\":true"),
        "inaccessible-mask ppoll did not meet canonical INFO parity: {verdict}",
    );
}

#[test]
fn ppoll_waits_use_nonblocking_probes_and_verify() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ppoll-simulation");
    fs::create_dir_all(&build_root).expect("failed to create ppoll guest build directory");
    let guest = build_root.join("ppoll_simulation");

    let mut compile = Command::new("cc");
    compile
        .args([
            "-O0", "-g", "-pthread", "-std=c11", "-Wall", "-Wextra", "-Werror",
        ])
        .arg(repository.join("tests/c/ppoll_simulation.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "ppoll guest compilation");

    let mut native_raw_timeout = Command::new("timeout");
    native_raw_timeout
        .args(["--kill-after", "5s", "30s"])
        .arg(&guest)
        .arg("raw-timeout-copyout");
    let native_raw_timeout_output =
        command_output(native_raw_timeout, "native raw ppoll timeout copyout");
    let native_raw_timeout_stdout = String::from_utf8_lossy(&native_raw_timeout_output.stdout);
    let native_raw_timeout_stderr = String::from_utf8_lossy(&native_raw_timeout_output.stderr);
    assert!(
        native_raw_timeout_stdout.contains("ppoll-simulation-ok"),
        "native raw ppoll timeout copyout omitted its success marker\nstdout:\n{native_raw_timeout_stdout}\nstderr:\n{native_raw_timeout_stderr}",
    );

    let mut native_masked_readonly_timeout = Command::new("timeout");
    native_masked_readonly_timeout
        .args(["--kill-after", "5s", "30s"])
        .arg(&guest)
        .arg("masked-readonly-timeout");
    let native_masked_readonly_timeout_output = command_output(
        native_masked_readonly_timeout,
        "native masked read-only ppoll timeout",
    );
    let native_masked_readonly_timeout_stdout =
        String::from_utf8_lossy(&native_masked_readonly_timeout_output.stdout);
    let native_masked_readonly_timeout_stderr =
        String::from_utf8_lossy(&native_masked_readonly_timeout_output.stderr);
    assert!(
        native_masked_readonly_timeout_stdout.contains("ppoll-simulation-ok"),
        "native masked read-only ppoll timeout omitted its success marker\nstdout:\n{native_masked_readonly_timeout_stdout}\nstderr:\n{native_masked_readonly_timeout_stderr}",
    );

    let masked_verdict_directory =
        tempfile::tempdir().expect("failed to create masked ppoll verdict directory");
    let masked_verdict = masked_verdict_directory.path().join("verify.json");
    let mut masked_verify_command = Command::new("timeout");
    masked_verify_command
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "--backend=ptrace",
            "run",
            "--strict",
            "--verify",
            "--verify-strict",
        ])
        .arg(format!("--verify-json={}", masked_verdict.display()))
        .args(["--base-env=minimal", "--"])
        .arg(&guest)
        .arg("masked-readonly-timeout");
    let masked_verify_output = command_output(
        masked_verify_command,
        "strict ptrace masked read-only ppoll timeout verification",
    );
    let masked_verify_stdout = String::from_utf8_lossy(&masked_verify_output.stdout);
    let masked_verify_stderr = String::from_utf8_lossy(&masked_verify_output.stderr);
    assert!(
        masked_verify_stdout.contains("Determinism verified")
            || masked_verify_stderr.contains("Determinism verified"),
        "Hermit omitted its masked read-only ppoll determinism marker\nstdout:\n{masked_verify_stdout}\nstderr:\n{masked_verify_stderr}",
    );
    let masked_verdict = fs::read_to_string(&masked_verdict)
        .expect("failed to read masked ppoll verification verdict");
    assert!(
        masked_verdict.contains("\"bitwise_parity\":true"),
        "masked read-only ppoll did not meet canonical INFO parity: {masked_verdict}",
    );

    let mut trace_command = Command::new("timeout");
    trace_command
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=trace", "run", "--strict", "--base-env=minimal", "--"])
        .arg(&guest);
    let trace_output = command_output(trace_command, "strict ppoll trace");
    let trace_stdout = String::from_utf8_lossy(&trace_output.stdout);
    let trace_stderr = String::from_utf8_lossy(&trace_output.stderr);
    assert!(
        trace_stdout.contains("ppoll-simulation-ok"),
        "ppoll guest omitted its success marker\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );
    assert!(
        trace_stderr.contains("InternalIOPolling")
            && trace_stderr.contains("Retry #1 for syscall due to result Ok(0)"),
        "ppoll did not use nonblocking scheduler probes\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );

    let trace_recording =
        tempfile::tempdir().expect("failed to create ppoll trace recording directory");
    let mut record_trace_command = Command::new("timeout");
    record_trace_command
        .args(["--kill-after", "5s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=trace",
            "--backend=ptrace",
            "record",
            "start",
            "--strict",
            "--record-timeout=30",
        ])
        .arg(format!("--data-dir={}", trace_recording.path().display()))
        .arg("--")
        .arg(&guest)
        .arg("masked-fail-closed");
    let record_trace_output = command_output(record_trace_command, "strict ppoll record trace");
    let record_trace_stdout = String::from_utf8_lossy(&record_trace_output.stdout);
    let record_trace_stderr = String::from_utf8_lossy(&record_trace_output.stderr);
    assert!(
        record_trace_stderr.lines().any(|line| {
            line.contains("Recorder observed ppoll input")
                && line.contains("has_signal_mask=true")
                && line.contains("timeout_is_zero=true")
        }),
        "masked zero-time ppoll probe did not reach Recorder\nstdout:\n{record_trace_stdout}\nstderr:\n{record_trace_stderr}",
    );
    assert!(
        !record_trace_stderr
            .lines()
            .any(|line| { line.contains("BlockingExternalIO") && line.contains("ppoll") }),
        "masked ppoll used a BlockingExternalIO resource\nstdout:\n{record_trace_stdout}\nstderr:\n{record_trace_stderr}",
    );

    for backend in ["ptrace", "dbt"] {
        let mut verify_command = Command::new("timeout");
        verify_command
            .args(["--kill-after", "5s", "30s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args(["--log=info", "run"])
            .arg(format!("--backend={backend}"))
            .args(["--strict", "--verify", "--base-env=minimal", "--"])
            .arg(&guest);
        let verify_output = command_output(
            verify_command,
            &format!("strict {backend} ppoll verification"),
        );
        let verify_stdout = String::from_utf8_lossy(&verify_output.stdout);
        let verify_stderr = String::from_utf8_lossy(&verify_output.stderr);
        assert!(
            verify_stdout.contains("Determinism verified")
                || verify_stderr.contains("Determinism verified"),
            "Hermit omitted its {backend} determinism marker\nstdout:\n{verify_stdout}\nstderr:\n{verify_stderr}",
        );
    }

    let recording = tempfile::tempdir().expect("failed to create ppoll recording directory");
    let verdict_directory = tempfile::tempdir().expect("failed to create ppoll verdict directory");
    let verdict = verdict_directory.path().join("verify.json");
    let mut replay_command = Command::new("timeout");
    replay_command
        .args(["--kill-after", "5s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "--backend=ptrace",
            "record",
            "start",
            "--strict",
            "--record-timeout=30",
            "--verify",
            "--verify-strict",
        ])
        .arg(format!("--verify-json={}", verdict.display()))
        .arg(format!("--data-dir={}", recording.path().display()))
        .arg("--")
        .arg(&guest)
        .arg("record-replay");
    let replay_output =
        command_output(replay_command, "strict ptrace ppoll record/replay scenario");
    let replay_stdout = String::from_utf8_lossy(&replay_output.stdout);
    let replay_stderr = String::from_utf8_lossy(&replay_output.stderr);
    assert!(
        replay_stdout.contains("Success: replay matched recording.")
            || replay_stderr.contains("Success: replay matched recording."),
        "Hermit omitted its ppoll replay marker\nstdout:\n{replay_stdout}\nstderr:\n{replay_stderr}",
    );
    let verdict = fs::read_to_string(&verdict).expect("failed to read ppoll replay verdict");
    assert!(
        verdict.contains("\"bitwise_parity\":true"),
        "ppoll record/replay did not meet canonical INFO parity: {verdict}",
    );
}
