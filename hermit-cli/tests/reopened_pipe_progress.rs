/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression test: reading a pipe that the guest REOPENED BY PATH must not
//! deadlock the scheduler.
//!
//! `/dev/stdin`, `/proc/self/fd/N` and bash's process substitution
//! (`< <(cmd)` reads `/dev/fd/63`) all reach an existing pipe through
//! `openat`, which creates a new open file description. `handle_pipe2` made the
//! original description physically nonblocking so that a read waiting for its
//! writer takes the deterministic nonblockize-and-retry path; the reopened
//! description had neither that O_NONBLOCK nor the Pipe type, so the read
//! blocked in the kernel while holding the scheduler turn and the writer never
//! ran (https://github.com/rrnewton/hermit/issues/1850). Every nixpkgs
//! fixupPhase does this (`while read ...; done < <(find ...)`).
//!
//! Each scenario makes the writer wait first, so the reader is certain to reach
//! an empty pipe. This asserts PROGRESS: the failure signal is `timeout`
//! killing the run (exit 124).
//!
//! The converse is pinned too: a pipe whose writer is a HOST process (hermit's
//! own stdin) is outside the scheduler, so reopening it must keep the
//! deterministic fill-the-buffer read rather than return whatever the host had
//! written so far.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::io::Write;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

/// A healthy run finishes in about a second. A deadlocked run never finishes,
/// so this bound only has to be generous enough to never fire on a loaded box.
const TIMEOUT_SECONDS: u64 = 60;

fn hermit_bash(extra: &[&str], script: &str) -> Command {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(hermit_test::hermit_binary())
        .args(["run", "--base-env=minimal", "--no-virtualize-cpuid"])
        .args(extra)
        .args(["--", "/bin/bash", "-c", script]);
    hermit_test::configure_guest_execution(&mut command);
    command
}

fn run_bash(extra: &[&str], script: &str, expected_stdout: &str) {
    let mut command = hermit_bash(extra, script);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start guest: {rendered}: {error}"));
    check_output(&rendered, &output, expected_stdout);
}

fn check_output(rendered: &str, output: &std::process::Output, expected_stdout: &str) {
    assert_ne!(
        output.status.code(),
        Some(124),
        "read from a reopened pipe deadlocked the scheduler (timed out): {rendered}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "guest failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        expected_stdout,
        "wrong guest output: {rendered}"
    );
}

const DEV_STDIN: &str = "{ /bin/sleep 0.2; echo hi; } | /bin/cat /dev/stdin";
const PROC_SELF_FD: &str = "{ /bin/sleep 0.2; echo hi; } | /bin/cat /proc/self/fd/0";
const PROCESS_SUBSTITUTION: &str = "n=0; while read -r line; do n=$((n+1)); done \
     < <(/bin/sleep 0.2; printf 'a\\nb\\nc\\n'); echo n=$n";

/// Measured against hermit 3629a4ec954d (before the fix): all three scenarios
/// in this file hang until `timeout` kills them; with the fix each exits 0.
#[test]
fn cat_dev_stdin_reads_a_pipe_whose_writer_is_late() {
    run_bash(&[], DEV_STDIN, "hi\n");
}

#[test]
fn cat_proc_self_fd_reads_a_pipe_whose_writer_is_late() {
    run_bash(&[], PROC_SELF_FD, "hi\n");
}

#[test]
fn process_substitution_reads_a_pipe_whose_writer_is_late() {
    run_bash(&[], PROCESS_SUBSTITUTION, "n=3\n");
}

#[test]
fn process_substitution_completes_under_strict() {
    run_bash(&["--strict"], PROCESS_SUBSTITUTION, "n=3\n");
}

/// L2 on the ptrace backend: the canonical comparator must report bitwise
/// parity over a nonzero number of compared INFO messages.
#[test]
fn process_substitution_passes_strict_verify() {
    let report_directory = tempfile::tempdir().expect("failed to create verify report directory");
    let report_path = report_directory.path().join("verify.json");
    let verify_json = format!("--verify-json={}", report_path.display());
    run_bash(
        &["--strict", "--verify", "--verify-strict", &verify_json],
        PROCESS_SUBSTITUTION,
        "n=3\n",
    );
    let report = std::fs::read_to_string(&report_path).expect("failed to read verify report");
    let report: serde_json::Value =
        serde_json::from_str(&report).expect("verify report is not JSON");
    assert_eq!(
        report["bitwise_parity"],
        serde_json::Value::Bool(true),
        "process substitution did not meet canonical parity: {report}"
    );
    for side in ["left", "right"] {
        assert!(
            report["compared_log_messages"][side]
                .as_u64()
                .is_some_and(|count| count > 0),
            "verify compared no {side} INFO messages: {report}"
        );
    }
}

/// Hermit's stdin is a HOST pipe whose writer sends one byte, pauses, then
/// sends the second. The guest reopens it as `/dev/stdin` and makes ONE
/// 4096-byte read (`dd count=1`). Deterministic IO fills that read until the
/// buffer is full or EOF, so the guest always sees both bytes. Typing the host
/// pipe as a scheduler-managed pipe would instead return the first byte alone,
/// a count that depends on host timing.
///
/// The test can only catch that regression if the guest's read starts during
/// the pause, so the pause is long: a healthy run reaches the read in well
/// under a second. It cannot fail falsely; a late read sees both bytes either
/// way.
#[test]
fn reopened_host_pipe_keeps_the_deterministic_full_read() {
    let mut command = hermit_bash(
        &[],
        "/bin/dd if=/dev/stdin bs=4096 count=1 2>/dev/null | /usr/bin/wc -c",
    );
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let rendered = format!("{command:?}");
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start guest: {rendered}: {error}"));
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin.write_all(b"a").expect("write the first byte");
    stdin.flush().expect("flush the first byte");
    std::thread::sleep(Duration::from_secs(3));
    // The guest may already have exited if it returned the short read.
    let _ = stdin.write_all(b"b");
    drop(stdin);
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("failed to wait for guest: {rendered}: {error}"));
    check_output(&rendered, &output, "2\n");
}
