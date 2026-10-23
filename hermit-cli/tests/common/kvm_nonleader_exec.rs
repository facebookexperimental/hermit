// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved. Licensed under the BSD-style license in LICENSE.

//! KVM's refused nonleader exec is unsupported, not successful backend parity.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
use hermit::canonical_verdict::Verdict;
use hermit::canonical_verdict::VerificationReport;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;
const DIAGNOSTIC: &str = "KVM nonleader exec is unsupported";

pub(super) fn run() {
    let _lock = super::hermit_run_guard();
    let _kvm = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .expect("the selected KVM refusal regression requires /dev/kvm");
    let start = Instant::now();
    let remaining = || {
        Duration::from_secs(55)
            .checked_sub(start.elapsed())
            .expect("the complete refusal regression must fit the 57-second test budget")
    };
    fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).expect("fixture parent");
    let root = tempfile::Builder::new()
        .prefix("kvm-nonleader-exec-refusal-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .keep();
    eprintln!("KVM nonleader exec refusal artifacts: {}", root.display());
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kvm_nonleader_exec.c");
    fs::copy(&fixture, root.join("guest.c")).unwrap();
    let guest = root.join("program");
    let status = bounded_command_with_timeout(
        Command::new("cc")
            .args([
                "-std=gnu11",
                "-O0",
                "-g",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-fno-pie",
                "-no-pie",
                "-pthread",
            ])
            .arg(&fixture)
            .arg("-o")
            .arg(&guest),
        &root.join("compile"),
        remaining(),
    );
    assert!(status.success(), "fixture compilation failed");
    let malformed = root.join("malformed");
    fs::write(&malformed, "not an executable image\n").unwrap();
    fs::set_permissions(&malformed, fs::Permissions::from_mode(0o755)).unwrap();
    let missing = root.join("missing");
    assert!(!missing.exists());

    // Native success proves the valid requests actually replace the worker.
    for mode in ["execve", "execveat"] {
        let directory = root.join(format!("native-{mode}"));
        let status =
            bounded_command_with_timeout(Command::new(&guest).arg(mode), &directory, remaining());
        assert_eq!(status.code(), Some(0));
        assert_eq!(
            bounded_read(&directory.join("stdout"), MIB),
            b"replacement-ran\n"
        );
        assert!(bounded_read(&directory.join("stderr"), MIB).is_empty());

        let directory = root.join(format!("refused-{mode}"));
        let logs = directory.join("verify-logs");
        fs::create_dir_all(&logs).unwrap();
        let report_path = directory.join("verification.json");
        let args = [
            "--log=info",
            "--backend=kvm",
            "run",
            "--base-env=minimal",
            "--tmp=/tmp",
            "--strict",
            "--epoch=2026-01-01T00:00:00.123456789+00:00",
            "--verify",
            "--verify-strict",
            "--verify-json",
            report_path.to_str().unwrap(),
            "--keep-logs",
            "--verify-log-dir",
            logs.to_str().unwrap(),
            "--",
            guest.to_str().unwrap(),
            mode,
        ];
        let mut command = super::hermit_command(&args);
        command.env("HERMIT_LOG_MAX_BYTES", (16 * MIB).to_string());
        let status = bounded_command_with_timeout(&mut command, &directory, remaining());
        let stderr = String::from_utf8(bounded_read(&directory.join("stderr"), 16 * MIB)).unwrap();
        // EXIT-CLASS: hermit
        assert_eq!(status.code(), Some(HERMIT_POLICY_REFUSAL_EXIT), "{stderr}");
        assert!(
            stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
            "{stderr}"
        );
        assert!(bounded_read(&directory.join("stdout"), MIB).is_empty());
        // Verification captures tool diagnostics in its retained first-run log.
        let mut diagnostic_found = stderr.contains(DIAGNOSTIC);
        for entry in fs::read_dir(&logs).unwrap() {
            let entry = entry.unwrap();
            assert!(entry.file_type().unwrap().is_file());
            let log = String::from_utf8(bounded_read(&entry.path(), 16 * MIB)).unwrap();
            diagnostic_found |= log.contains(DIAGNOSTIC);
        }
        assert!(
            diagnostic_found,
            "specific unsupported reason must survive: {stderr}"
        );
        let report = VerificationReport::from_json_slice(&bounded_read(&report_path, MIB)).unwrap();
        assert_eq!(report.verdict, Verdict::NoResult);
        assert!(!report.verified);
        assert!(!report.bitwise_parity);
        assert!(report.compared_outputs.is_none());
        assert!(report.compared_log_messages.is_none());
        assert!(report.require_canonical_match().is_err());
    }

    for (name, mode, opt_out, expected) in [
        (
            "ordinary-errors",
            "errors",
            false,
            "failed-exec-errors-preserved\n",
        ),
        ("leader-success", "leader", false, "replacement-ran\n"),
        (
            "compatibility-refusal",
            "execve",
            true,
            "unsupported-returned-enosys\n",
        ),
    ] {
        let directory = root.join(name);
        let mut args = vec![
            "--log=info",
            "--backend=kvm",
            "run",
            "--base-env=minimal",
            "--tmp=/tmp",
            "--epoch=2026-01-01T00:00:00.123456789+00:00",
            if opt_out {
                "--allow-unsupported-syscalls"
            } else {
                "--strict"
            },
            "--",
            guest.to_str().unwrap(),
            mode,
        ];
        if mode == "errors" {
            args.extend([malformed.to_str().unwrap(), missing.to_str().unwrap()]);
        }
        let mut command = super::hermit_command(&args);
        command.env("HERMIT_LOG_MAX_BYTES", (16 * MIB).to_string());
        let status = bounded_command_with_timeout(&mut command, &directory, remaining());
        let stderr = String::from_utf8(bounded_read(&directory.join("stderr"), 16 * MIB)).unwrap();
        // EXIT-CLASS: guest
        assert_eq!(status.code(), Some(0), "{name}: {stderr}");
        assert_eq!(
            bounded_read(&directory.join("stdout"), MIB),
            expected.as_bytes()
        );
        if opt_out {
            assert!(stderr.contains(DIAGNOSTIC), "{stderr}");
            assert!(
                stderr.contains("syscalls execveat used but not yet supported"),
                "{stderr}"
            );
            assert!(
                stderr.contains(
                    "a successful exit does not establish complete deterministic execution"
                ),
                "{stderr}"
            );
        } else {
            assert!(!stderr.contains(DIAGNOSTIC), "{name}: {stderr}");
            assert!(
                !stderr.contains("used but not yet supported"),
                "{name}: {stderr}"
            );
        }
    }
}
