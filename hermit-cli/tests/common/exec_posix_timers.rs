// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! Linux deletes POSIX timers at successful exec, but preserves ITIMER_REAL.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use detcore::Digest;
use hermit::HERMIT_INTERNAL_FAILURE_EXIT;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::NoResultReason;
use hermit::canonical_verdict::Verdict;
use hermit::canonical_verdict::VerificationReport;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;
const EXPECTED: &[u8] = b"PASS exec deleted POSIX timers; failed exec retained timers; ITIMER_REAL survived; clock advanced\n";
const FAILED_EXEC_MARKER: &[u8] = b"failed exec returned ENOENT\n";

pub(super) fn run(backend: &str) {
    let _lock = super::hermit_run_guard();
    if backend == "kvm" {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .expect("selected KVM exec timer regression requires /dev/kvm; absence is not a pass");
    }
    let root = tempfile::Builder::new()
        .prefix(&format!("exec-posix-timers-{backend}-"))
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained exec timer fixture directory")
        .keep();
    eprintln!(
        "{backend} exec timer artifacts retained at {}",
        root.display()
    );
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/exec_posix_timers.c");
    fs::copy(&fixture, root.join("guest.c")).expect("retain exact fixture");
    let guest = root.join("program");
    let compile = root.join("compile");
    let status = bounded_command_with_timeout(
        Command::new("cc")
            .args([
                "-std=gnu11",
                "-O2",
                "-g",
                "-Wall",
                "-Wextra",
                "-Wpedantic",
                "-Wformat=2",
                "-Werror",
                "-fno-pie",
                "-no-pie",
            ])
            .arg(&fixture)
            .arg("-o")
            .arg(&guest),
        &compile,
        Duration::from_secs(20),
    );
    assert!(status.success(), "exec timer fixture compilation failed");
    assert!(bounded_read(&compile.join("stdout"), 64 * MIB).is_empty());
    assert!(bounded_read(&compile.join("stderr"), 16 * MIB).is_empty());

    let directory = root.join("verify");
    let logs = directory.join("verify-logs");
    fs::create_dir_all(&logs).expect("retained verification logs");
    let report_path = directory.join("verification.json");
    let args = [
        "--log=info",
        "--backend",
        backend,
        "run",
        "--base-env=minimal",
        "--strict",
        "--verify-strict",
        "--verify",
        "--verify-json",
        report_path.to_str().unwrap(),
        "--keep-logs",
        "--verify-log-dir",
        logs.to_str().unwrap(),
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
        "--env=LC_ALL=C",
        "--env=TZ=UTC",
        "--",
        guest.to_str().unwrap(),
    ];
    let mut command = super::hermit_command(&args);
    command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
    let status = bounded_command_with_timeout(&mut command, &directory, Duration::from_secs(57));
    assert_eq!(status.code(), Some(0), "{backend} exec timer guest status");
    assert_eq!(
        bounded_read(&directory.join("stdout"), 64 * MIB),
        EXPECTED,
        "{backend} exec timer guest must complete every assertion"
    );
    let report = VerificationReport::from_json_slice(&bounded_read(&report_path, 16 * MIB))
        .expect("complete typed verification report");
    report
        .require_canonical_match()
        .expect("full nonempty canonical INFO match");
    report
        .require_exact_output_match()
        .expect("exact two-run status/stdout/stderr match");
    assert_eq!(report.guest_exit_code, Some(0));
    assert!(report.guest_signal.is_none());
    let policy = report.comparison.as_ref().unwrap();
    assert_eq!(policy.display_name.as_deref(), Some("BitwiseInfoV1"));
    assert_eq!(policy.compare_io_buffers, Some(true));
    assert_eq!(policy.virtualize_time, Some(true));
    assert_eq!(policy.strip_lines, Some(false));
    assert_eq!(policy.canonicalize_addresses, Some(true));
    assert_eq!(policy.full_trace, Some(true));
    assert_eq!(policy.exact_remainder, Some(true));
    assert_eq!(policy.ignore_lines, Some(false));
    assert_eq!(policy.skip_commit, Some(false));
    assert_eq!(policy.skip_detlog, Some(false));
    assert_eq!(policy.log_scope, Some(ComparedLogScope::Info));
    let outputs = report.compared_outputs.as_ref().unwrap();
    for operand in [&outputs.left, &outputs.right] {
        assert_eq!(operand.exit_code, Some(0));
        assert!(operand.signal.is_none());
        assert_eq!(operand.stdout_bytes, EXPECTED.len() as u64);
        assert_eq!(operand.stdout_sha256, Digest::new(EXPECTED).to_string());
        assert_eq!(operand.stderr_bytes, 0);
        assert_eq!(operand.stderr_sha256, Digest::new(b"").to_string());
    }
    let retained = |prefix: &str| -> Vec<_> {
        fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
            .collect()
    };
    // After a match `--keep-logs` keeps only run 1's log, the golden copy;
    // run 2's log, which matched it, is deleted.
    let golden = retained("run1_log_");
    assert_eq!(
        golden.len(),
        1,
        "one retained golden log of the matched guest"
    );
    assert!(!bounded_read(&golden[0], 64 * MIB).is_empty());
    assert!(
        retained("run2_log_").is_empty(),
        "a matched verification must not retain run 2's log"
    );
    run_failed_exec_expiry(backend, &guest, &root);
}

/// This is a deadline-liveness control, separate from the survivor's L2 proof.
/// The default fatal signal intentionally prevents verification from finishing;
/// a matching signal and the persisted post-exec marker are required together.
fn run_failed_exec_expiry(backend: &str, guest: &Path, root: &Path) {
    let directory = root.join("failed-exec-expiry");
    let logs = directory.join("verify-logs");
    let evidence = directory.join("evidence");
    fs::create_dir_all(&logs).expect("retained failed-exec verification logs");
    fs::create_dir(&evidence).expect("fresh failed-exec evidence directory");
    let marker = evidence.join("after-failed-exec");
    assert!(
        !marker.exists(),
        "failed-exec marker must not predate this run"
    );
    let report_path = directory.join("verification.json");
    let mount = format!(
        "--mount=type=bind,source={},target=/tmp/exec-timer-control",
        evidence.display()
    );
    let args = [
        "--log=info",
        "--backend",
        backend,
        "run",
        "--base-env=minimal",
        "--strict",
        "--verify-strict",
        "--verify",
        "--verify-json",
        report_path.to_str().unwrap(),
        "--keep-logs",
        "--verify-log-dir",
        logs.to_str().unwrap(),
        "--mount=type=tmpfs,target=/test",
        &mount,
        "--workdir=/test",
        "--env=LC_ALL=C",
        "--env=TZ=UTC",
        "--",
        guest.to_str().unwrap(),
        "failed-exec-expiry",
        "/tmp/exec-timer-control/after-failed-exec",
    ];
    let mut command = super::hermit_command(&args);
    command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
    let status = bounded_command_with_timeout(&mut command, &directory, Duration::from_secs(57));
    assert_eq!(status.code(), Some(HERMIT_INTERNAL_FAILURE_EXIT));
    assert_eq!(bounded_read(&marker, 1024), FAILED_EXEC_MARKER);
    assert!(bounded_read(&directory.join("stdout"), 64 * MIB).is_empty());
    let report = VerificationReport::from_json_slice(&bounded_read(&report_path, 16 * MIB))
        .expect("typed failed-exec timer disposition");
    assert_eq!(report.verdict, Verdict::NoResult);
    assert!(!report.verified);
    assert!(!report.bitwise_parity);
    assert!(report.comparison.is_none());
    assert!(report.compared_log_messages.is_none());
    assert!(report.compared_outputs.is_none());
    assert!(report.infrastructure_error.is_none());
    assert!(report.guest_exit_code.is_none());
    // Both backends deliver the timer's SIGUSR2 to the guest, whose default
    // action kills it: a POSIX timer expiry reaches a KVM guest through the
    // backend's process signal control, as a ptrace guest's reaches it
    // through the kernel.
    let expected_reason = match backend {
        "ptrace" | "kvm" => {
            assert_eq!(report.guest_signal, Some(libc::SIGUSR2));
            NoResultReason::FirstRunRejected {
                exit_code: None,
                signal: Some(libc::SIGUSR2),
                stdout_bytes: 0,
                stderr_bytes: 0,
            }
        }
        _ => panic!("unsupported exec timer test backend: {backend}"),
    };
    assert_eq!(report.no_result_reason, Some(expected_reason));
}
