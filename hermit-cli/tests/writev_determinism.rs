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
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

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

fn kill_process_group(child: &mut std::process::Child) {
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
}

fn run_two_blocked_writers(guest: &Path) {
    let mut command = Command::new(hermit_test::hermit_binary());
    command
        .args([
            "--log=trace",
            "run",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(guest)
        .arg("two-blocked-writers");
    hermit_test::configure_guest_execution(&mut command);
    command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let rendered = format!("{command:?}");
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn two-writer probe: {rendered}: {error}"));
    let mut child_stdin = Some(child.stdin.take().expect("stdin was piped"));
    let stderr = child.stderr.take().expect("stderr was piped");
    let (send, receive) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if send.send(line.map_err(|error| error.to_string())).is_err() {
                return;
            }
        }
    });

    let started = Instant::now();
    let mut scalar_retried = false;
    let mut vectored_retried = false;
    let mut diagnostics = Vec::new();
    let mut failure = None;
    let status = loop {
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(Ok(line)) => {
                scalar_retried |= line.contains("Retry #1 for blocking pipe write:");
                vectored_retried |=
                    line.contains("Retry #1 for blocking pipe writev after EAGAIN:");
                if diagnostics.len() < 2_000 {
                    diagnostics.push(line);
                }
            }
            Ok(Err(error)) => {
                failure = Some(format!("failed while draining Hermit stderr: {error}"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }

        if scalar_retried
            && vectored_retried
            && let Some(mut stdin) = child_stdin.take()
            && let Err(error) = stdin.write_all(b"x")
        {
            failure = Some(format!("failed to release the two-writer guest: {error}"));
        }

        if let Some(reason) = failure.as_ref() {
            kill_process_group(&mut child);
            break child.wait().unwrap_or_else(|error| {
                panic!("failed to reap two-writer probe after {reason}: {error}")
            });
        }
        if let Some(status) = child.try_wait().expect("failed to poll two-writer probe") {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(30) {
            failure = Some(format!(
                "two-writer probe timed out: scalar_retried={scalar_retried}, \
                 vectored_retried={vectored_retried}"
            ));
        }
    };
    reader.join().expect("stderr reader panicked");
    while let Ok(line) = receive.try_recv() {
        match line {
            Ok(line) if diagnostics.len() < 2_000 => diagnostics.push(line),
            Ok(_) => {}
            Err(error) => panic!("failed while draining Hermit stderr: {error}"),
        }
    }
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout was piped")
        .read_to_string(&mut stdout)
        .expect("failed to read two-writer stdout");
    let stderr = diagnostics.join("\n");
    if let Some(reason) = failure {
        panic!("{reason}\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }
    assert!(
        status.success(),
        "two-writer probe failed: {rendered}\nstatus: {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        scalar_retried && vectored_retried,
        "reader was not released until both retries, but the retained evidence is incomplete: \
         scalar_retried={scalar_retried}, vectored_retried={vectored_retried}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        stdout.contains("two-blocked-pipe-writers-ok"),
        "two-writer guest omitted its success marker\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
}

fn compile_writev_guest(build_name: &str) -> std::path::PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(build_name);
    fs::create_dir_all(&build_root).expect("failed to create writev guest build directory");
    let guest = build_root.join("writev_determinism");

    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/writev_determinism.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "writev guest compilation");
    guest
}

#[test]
fn writev_uses_fd_aware_scheduling_and_verifies() {
    let guest = compile_writev_guest("writev-determinism");
    let build_root = guest.parent().expect("writev guest should have a parent");

    let mut trace = Command::new("timeout");
    trace
        .args(["--kill-after", "5s", "30s"])
        .arg(hermit_test::hermit_binary())
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
        trace_stdout.contains("writev-signal-interrupt-ok"),
        "a sibling pthread_kill did not interrupt full-pipe writev with EINTR\n\
         stdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );
    assert!(
        trace_stdout.contains("writev-signal-restart-ok"),
        "SA_RESTART did not resume full-pipe writev after the pipe was drained\n\
         stdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );
    for marker in [
        "writev-partial-caught-ok",
        "writev-partial-blocked-ok",
        "writev-partial-ignored-ok",
        "writev-partial-mixed-ok",
    ] {
        assert!(
            trace_stdout.contains(marker),
            "writev guest omitted {marker}\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
        );
    }
    assert!(
        trace_stderr.contains("inbound syscall: writev")
            && trace_stderr.contains(
                "NonblockableSyscall: converting to nonblocking syscall (internal polling): write",
            )
            && trace_stderr.contains(
                "NonblockableSyscall: converting to nonblocking syscall (internal polling): writev",
            )
            && trace_stderr.contains("Retry #1 for blocking pipe write")
            && trace_stderr.contains("Retry #1 for atomic blocking pipe writev after EAGAIN"),
        "write/writev did not reach typed dispatch and internal-fd scheduling\n\
         stdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );

    // Keep the reader stopped until the host-side harness has observed both
    // writer threads retrying against the full pipe. This proves both writes
    // blocked; an entered counter before each syscall cannot establish that.
    run_two_blocked_writers(&guest);

    for (label, strict, extra_arg) in [
        ("strict writev verification", true, None),
        (
            "passthru-opt writev verification",
            false,
            Some("--passthru-opt"),
        ),
    ] {
        let report_dir = tempfile::tempdir().expect("failed to create verification directory");
        let report_path = report_dir.path().join("verify.json");
        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "30s"])
            .arg(hermit_test::hermit_binary())
            .args(["--log=info", "run", "--verify", "--base-env=minimal"]);
        if strict {
            verify
                .args([
                    "--strict",
                    "--panic-on-unsupported-syscalls",
                    "--verify-strict",
                ])
                .arg(format!("--verify-json={}", report_path.display()));
        }
        if let Some(arg) = extra_arg {
            verify.args(["--allow-unsupported-syscalls", arg]);
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
        if !strict {
            assert!(
                verify_stderr.contains(
                    "a successful exit does not establish complete deterministic execution",
                ),
                "passthru-opt writev run omitted the compatibility warning\n\
                 stderr:\n{verify_stderr}",
            );
        } else {
            let report: serde_json::Value = serde_json::from_slice(
                &fs::read(&report_path).expect("strict write verification report was not written"),
            )
            .expect("strict write verification report is valid JSON");
            assert_eq!(report["verdict"], "matched", "verify report: {report}");
            assert_eq!(report["verified"], true, "verify report: {report}");
            assert_eq!(report["bitwise_parity"], true, "verify report: {report}");
            assert_eq!(
                report["comparison"]["strictness"], "canonical",
                "verify report: {report}"
            );
            assert_eq!(
                report["comparison"]["log_scope"], "info",
                "verify report: {report}"
            );
            assert_eq!(
                report["comparison"]["compare_io_buffers"], true,
                "verify report: {report}"
            );
            for side in ["left", "right"] {
                assert!(
                    report["compared_log_messages"][side]
                        .as_u64()
                        .is_some_and(|count| count > 0),
                    "verify report compared no INFO evidence on {side}: {report}"
                );
            }
        }
    }

    // A blocking write that yielded on a full pipe must not resume through a
    // descriptor number that another thread replaced. Reverie does not yet
    // expose a backend-neutral retained-fd handle, so the safe current behavior
    // is an explicit EOPNOTSUPP before any byte reaches the replacement pipe.
    let replacement_report_dir =
        tempfile::tempdir().expect("failed to create replacement verification directory");
    let replacement_report = replacement_report_dir.path().join("verify.json");
    let mut replacement = Command::new("timeout");
    replacement
        .args(["--kill-after", "5s", "30s"])
        .arg(hermit_test::hermit_binary())
        // NO `--strict` HERE, and the CLI is why: it refuses `--strict` together
        // with `--allow-unsupported-syscalls`, and this case cannot drop the
        // latter -- observing the EOPNOTSUPP refusal IS the case. Written with
        // both, this invocation exited 2 on the argument conflict and so had
        // never run. The canonical comparison is retained through
        // `--verify-strict` and the JSON report below.
        .args([
            "--log=info",
            "run",
            "--verify",
            "--verify-strict",
            "--allow-unsupported-syscalls",
            "--base-env=minimal",
        ])
        .arg(format!("--verify-json={}", replacement_report.display()))
        .arg("--")
        .arg(&guest)
        .arg("fd-replacement");
    let replacement_output = command_output(replacement, "pipe fd replacement verification");
    let replacement_stdout = String::from_utf8_lossy(&replacement_output.stdout);
    let replacement_stderr = String::from_utf8_lossy(&replacement_output.stderr);
    assert!(
        replacement_stdout.contains("pipe-fd-replacement-refused-without-redirection"),
        "fd replacement probe omitted its success marker\n\
         stdout:\n{replacement_stdout}\nstderr:\n{replacement_stderr}",
    );
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&replacement_report)
            .expect("pipe fd replacement verification report was not written"),
    )
    .expect("pipe fd replacement verification report is valid JSON");
    assert_eq!(report["verdict"], "matched", "verify report: {report}");
    assert_eq!(report["verified"], true, "verify report: {report}");
    assert_eq!(report["bitwise_parity"], true, "verify report: {report}");
    assert_eq!(
        report["comparison"]["strictness"], "canonical",
        "verify report: {report}"
    );
    assert_eq!(
        report["comparison"]["compare_io_buffers"], true,
        "verify report: {report}"
    );

    // Exercise record mode on pipe retries separately. Replaying dynamically allocated
    // pipe fds is currently blocked by the recorder's independent fd-numbering gap.
    let pipe_recording = build_root.join("pipe-recording");
    let _ = fs::remove_dir_all(&pipe_recording);
    let mut pipe_record = Command::new("timeout");
    pipe_record
        .args(["--kill-after", "5s", "60s"])
        .arg(hermit_test::hermit_binary())
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
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
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
