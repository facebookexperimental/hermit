/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;

use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
use hermit::HERMIT_INTERNAL_FAILURE_EXIT;
use hermit::run_evidence::DispositionLimitation;
use hermit::run_evidence::GuestDisposition;
use hermit::run_evidence::GuestRunResult;
use hermit::run_evidence::RunEvidenceBackend;
use hermit::run_evidence::RunEvidenceInspection;
use hermit::run_evidence::RunEvidenceInspectionFailure;
use hermit::run_evidence::RunEvidenceNoResultReason;
use hermit::run_evidence::RunEvidenceOutcome;
use hermit::run_evidence::inspect_run_evidence;

#[path = "common/hermit_binary.rs"]
mod hermit_test;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

fn hermit_run_guard() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn compile_c_fixture(source: &str, build_root: &Path, output: &str) -> PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    fs::create_dir_all(build_root).expect("failed to create fixture directory");
    let guest = build_root.join(output);
    let compiled = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join(source))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile run-evidence fixture");
    assert!(
        compiled.status.success(),
        "fixture compilation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compiled.stdout),
        String::from_utf8_lossy(&compiled.stderr),
    );
    guest
}

fn session_identity_guest(parent: &Path) -> PathBuf {
    compile_c_fixture(
        "tests/c/session_identity.c",
        &parent.join("run-evidence-session-identity"),
        "session_identity",
    )
}

fn stdio_identity_guest(parent: &Path) -> PathBuf {
    compile_c_fixture(
        "tests/c/stdio_lseek_identity.c",
        &parent.join("run-evidence-stdio-identity"),
        "stdio_lseek_identity",
    )
}

fn hermit_command() -> Command {
    Command::new(hermit_test::hermit_binary())
}

fn prepare_command_for(command: &mut Command, requested: Option<&OsStr>) -> Result<(), String> {
    hermit_test::configure_guest_execution_for(command, requested)
}

// All callers configure arguments and explicit environment entries only. Apply
// staging before output configures its standard descriptors.
fn command_output(command: &mut Command) -> std::io::Result<Output> {
    let requested = std::env::var_os("HERMIT_E2E_EMPTY_WORKDIR");
    prepare_command_for(command, requested.as_deref())
        .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}"));
    command.output()
}

fn run(args: &[&str]) -> Output {
    command_output(hermit_command().args(args))
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"))
}

#[test]
fn run_evidence_command_staging_is_exact() {
    let selected = hermit_test::hermit_binary();
    if let Some(path) = std::env::var_os("HERMIT_BIN").filter(|path| !path.is_empty()) {
        assert_eq!(selected.as_os_str(), path);
    }
    for (args, cap) in [
        (
            vec![
                "run",
                "--run-evidence-dir",
                "/fixture/evidence",
                "--",
                "/bin/printf",
                "",
                "quoted \"value\"\n$HOME",
            ],
            None,
        ),
        (
            vec![
                "--log-file",
                "/fixture/baseline.log",
                "run",
                "--tmp=/tmp",
                "--",
                "/fixture/guest",
            ],
            None,
        ),
        (
            vec![
                "--log-file",
                "/fixture/public.log",
                "run",
                "--tmp=/tmp",
                "--run-evidence-dir",
                "/fixture/evidence",
                "--",
                "/fixture/guest",
            ],
            None,
        ),
        (
            vec![
                "run",
                "--run-evidence-dir",
                "/fixture/evidence",
                "--",
                "/bin/true",
            ],
            Some("1"),
        ),
        (vec!["run", "--help"], None),
    ] {
        let original = args.iter().map(OsString::from).collect::<Vec<_>>();
        for requested in [None, Some(OsStr::new("/test"))] {
            let mut command = hermit_command();
            command.args(&args).current_dir("/fixture/controller");
            if let Some(cap) = cap {
                command.env("HERMIT_LOG_MAX_BYTES", cap);
            }
            prepare_command_for(&mut command, requested).unwrap();
            let mut expected = original.clone();
            if requested.is_some()
                && let Some(separator) = expected.iter().position(|arg| arg == "--")
            {
                expected.splice(
                    separator..separator,
                    [
                        OsString::from("--base-env=minimal"),
                        OsString::from("--mount=type=tmpfs,target=/test"),
                        OsString::from("--workdir=/test"),
                    ],
                );
            }
            assert_eq!(command.get_program(), selected);
            assert_eq!(command.get_args().collect::<Vec<_>>(), expected);
            assert_eq!(
                command.get_current_dir(),
                Some(Path::new("/fixture/controller"))
            );
            let expected_env =
                cap.map(|cap| (OsStr::new("HERMIT_LOG_MAX_BYTES"), Some(OsStr::new(cap))));
            assert_eq!(
                command.get_envs().collect::<Vec<_>>(),
                expected_env.into_iter().collect::<Vec<_>>()
            );
        }
    }

    let mut invalid = hermit_command();
    invalid.args(["run", "--", "/bin/true"]);
    let original = format!("{invalid:?}");
    assert!(prepare_command_for(&mut invalid, Some(OsStr::new("/wrong"))).is_err());
    assert_eq!(format!("{invalid:?}"), original);
}

#[test]
fn run_evidence_command_staging_uses_the_selected_binary() {
    let parent = tempfile::tempdir().unwrap();
    let selected = parent.path().join("selected hermit (not executed)");
    assert_ne!(selected, Path::new(env!("CARGO_BIN_EXE_hermit")));
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "run_evidence_command_staging_is_exact",
            "--nocapture",
        ])
        .env("HERMIT_BIN", &selected)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored;"));
    assert!(
        !selected.exists(),
        "pure command control launched its selected path"
    );
}

#[test]
fn help_describes_the_no_clobber_backend_contract() {
    let output = run(&["run", "--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for required in [
        "--run-evidence-dir <NEW_PATH>",
        "must not exist",
        "supported by ptrace, LiteInst",
        "and KVM",
        "BitwiseInfoV1",
        "isolated process-group policy",
        "SaBRe",
        "exit-code-only",
    ] {
        assert!(
            help.contains(required),
            "help omitted {required:?}:\n{help}"
        );
    }
}

#[test]
fn preexisting_destination_is_unchanged_and_guest_does_not_launch() {
    let _guard = hermit_run_guard();
    let parent = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let destination = parent.path().join("existing");
    fs::create_dir(&destination).unwrap();
    let sentinel = destination.join("sentinel");
    fs::write(&sentinel, b"keep-me").unwrap();
    let launched = parent.path().join("guest-launched");
    let destination_arg = destination.display().to_string();
    let launched_arg = launched.display().to_string();

    let output = run(&[
        "run",
        "--run-evidence-dir",
        &destination_arg,
        "--",
        "/bin/sh",
        "-c",
        "printf launched > \"$1\"",
        "sh",
        &launched_arg,
    ]);
    assert_eq!(output.status.code(), Some(HERMIT_INTERNAL_FAILURE_EXIT));
    assert!(!launched.exists(), "guest ran despite destination refusal");
    assert_eq!(fs::read(&sentinel).unwrap(), b"keep-me");
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
}

#[test]
fn dbt_and_sabre_evidence_are_refused_before_guest_launch() {
    let _guard = hermit_run_guard();
    for (backend, expected) in [
        ("dbt", "isolated process group"),
        ("sabre", "shared stderr"),
    ] {
        let parent = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let destination = parent.path().join("evidence");
        let launched = parent.path().join("guest-launched");
        let destination_arg = destination.display().to_string();
        let launched_arg = launched.display().to_string();
        let output = run(&[
            "run",
            "--backend",
            backend,
            "--run-evidence-dir",
            &destination_arg,
            "--",
            "/bin/sh",
            "-c",
            "printf launched > \"$1\"",
            "sh",
            &launched_arg,
        ]);

        assert_eq!(output.status.code(), Some(HERMIT_POLICY_REFUSAL_EXIT));
        assert!(
            !launched.exists(),
            "{backend} guest launched before refusal"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{backend} refusal omitted its semantic limit: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
                RunEvidenceNoResultReason::UnsupportedBackend
            ))
        );
    }
}

#[test]
fn sidecar_preserves_stdout_stderr_status_and_reports_nonzero_info() {
    let _guard = hermit_run_guard();
    let parent = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let destination = parent.path().join("evidence");
    let destination_arg = destination.display().to_string();
    let guest = [
        "/bin/sh",
        "-c",
        "printf ordinary-out; printf ordinary-err >&2; exit 23",
    ];

    let mut baseline_args = vec!["run", "--"];
    baseline_args.extend(guest);
    let baseline = run(&baseline_args);
    let mut evidence_args = vec!["run", "--run-evidence-dir", &destination_arg, "--"];
    evidence_args.extend(guest);
    let with_evidence = run(&evidence_args);

    assert_eq!(with_evidence.status, baseline.status);
    assert_eq!(with_evidence.stdout, baseline.stdout);
    assert_eq!(with_evidence.stderr, baseline.stderr);
    let RunEvidenceInspection::Complete(report) = inspect_run_evidence(&destination) else {
        panic!("ordinary ptrace evidence did not validate")
    };
    assert_eq!(report.backend, RunEvidenceBackend::Ptrace);
    assert_eq!(report.attempt, 1);
    assert_ne!(report.invocation_id, uuid::Uuid::nil());
    assert!(report.canonical_info.message_count > 0);
    assert!(report.canonical_info.byte_count > 0);
    assert!(report.canonical_info.sha256.is_some());
    assert_eq!(
        report.outcome,
        RunEvidenceOutcome::Complete {
            disposition: GuestDisposition::Exited { code: 23 }
        }
    );
}

#[test]
fn sidecar_preserves_session_and_process_group_identity() {
    let _guard = hermit_run_guard();
    let parent = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let destination = parent.path().join("evidence");
    let destination_arg = destination.display().to_string();
    let guest_path = session_identity_guest(parent.path());
    let guest = guest_path.to_str().unwrap();

    // The test binary can itself live below /tmp when this suite is built in a
    // disposable mirror. Expose that host path identically in both controls.
    let baseline = run(&["run", "--tmp=/tmp", "--", guest]);
    let with_evidence = run(&[
        "run",
        "--tmp=/tmp",
        "--run-evidence-dir",
        &destination_arg,
        "--",
        guest,
    ]);

    assert!(
        baseline.status.success(),
        "{}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    assert_eq!(with_evidence.status, baseline.status);
    assert_eq!(with_evidence.stdout, baseline.stdout);
    assert_eq!(with_evidence.stderr, baseline.stderr);
    let stdout = String::from_utf8(with_evidence.stdout).unwrap();
    assert!(stdout.contains("setpgid rc=0 errno=0"));
    assert!(stdout.contains("setsid rc=-1 errno=1"));
    assert!(matches!(
        inspect_run_evidence(&destination),
        RunEvidenceInspection::Complete(_)
    ));
}

#[test]
fn private_evidence_does_not_reuse_the_public_log_file_or_add_a_worker() {
    let _guard = hermit_run_guard();
    let parent = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let baseline_log = parent.path().join("baseline.log");
    let public_log = parent.path().join("public.log");
    let evidence = parent.path().join("evidence");
    let guest_path = session_identity_guest(parent.path());
    let guest = guest_path.to_str().unwrap();

    let baseline = command_output(
        hermit_command()
            .args(["--log-file"])
            .arg(&baseline_log)
            .args(["run", "--tmp=/tmp", "--", guest]),
    )
    .unwrap();
    let with_evidence = command_output(
        hermit_command()
            .args(["--log-file"])
            .arg(&public_log)
            .args(["run", "--tmp=/tmp", "--run-evidence-dir"])
            .arg(&evidence)
            .args(["--", guest]),
    )
    .unwrap();

    assert!(
        baseline.status.success(),
        "{}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    assert_eq!(with_evidence.status, baseline.status);
    assert_eq!(with_evidence.stdout, baseline.stdout);
    assert_eq!(with_evidence.stderr, baseline.stderr);
    assert_eq!(
        fs::read(&public_log).unwrap(),
        fs::read(&baseline_log).unwrap(),
        "the private INFO layer changed the default-WARN public log"
    );

    let RunEvidenceInspection::Complete(report) = inspect_run_evidence(&evidence) else {
        panic!("ordinary ptrace evidence did not validate")
    };
    let artifact = evidence.join(&report.canonical_info.artifact);
    assert!(!fs::read(&artifact).unwrap().is_empty());
    let public_metadata = fs::metadata(&public_log).unwrap();
    let artifact_metadata = fs::metadata(&artifact).unwrap();
    assert_ne!(
        (public_metadata.dev(), public_metadata.ino()),
        (artifact_metadata.dev(), artifact_metadata.ino()),
        "private evidence must not alias --log-file"
    );
}

#[test]
fn sidecar_does_not_replace_or_reopen_guest_standard_descriptors() {
    let _guard = hermit_run_guard();
    let parent = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let baseline_report = parent.path().join("baseline-report");
    let baseline_guest_output = parent.path().join("baseline-guest-output");
    let evidence_report = parent.path().join("evidence-report");
    let evidence_guest_output = parent.path().join("evidence-guest-output");
    let evidence = parent.path().join("evidence");
    let guest_path = stdio_identity_guest(parent.path());
    let guest = guest_path.to_str().unwrap();

    let baseline = run(&[
        "run",
        "--tmp=/tmp",
        "--",
        guest,
        baseline_report.to_str().unwrap(),
        baseline_guest_output.to_str().unwrap(),
    ]);
    let with_evidence = run(&[
        "run",
        "--tmp=/tmp",
        "--run-evidence-dir",
        evidence.to_str().unwrap(),
        "--",
        guest,
        evidence_report.to_str().unwrap(),
        evidence_guest_output.to_str().unwrap(),
    ]);

    assert!(
        baseline.status.success(),
        "{}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    assert_eq!(with_evidence.status, baseline.status);
    assert_eq!(with_evidence.stdout, baseline.stdout);
    assert_eq!(with_evidence.stderr, baseline.stderr);
    assert_eq!(
        fs::read(&evidence_report).unwrap(),
        fs::read(&baseline_report).unwrap()
    );
    assert_eq!(
        fs::read(&evidence_guest_output).unwrap(),
        fs::read(&baseline_guest_output).unwrap()
    );
}

#[test]
fn truncated_private_log_is_terminal_no_result_without_changing_guest_status() {
    let _guard = hermit_run_guard();
    let parent = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let destination = parent.path().join("evidence");
    let output = command_output(
        hermit_command()
            .env("HERMIT_LOG_MAX_BYTES", "1")
            .args(["run", "--run-evidence-dir"])
            .arg(&destination)
            .args(["--", "/bin/true"]),
    )
    .unwrap();

    assert!(output.status.success());
    assert_eq!(
        inspect_run_evidence(&destination),
        RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
            RunEvidenceNoResultReason::TruncatedCanonicalInfo
        ))
    );
}

#[test]
fn kvm_schema_never_claims_signal_precision() {
    let disposition = GuestDisposition::ExitCodeOnly {
        code: 0,
        limitation: DispositionLimitation::KvmExitCodeOnly,
    };
    let value = serde_json::to_value(disposition).unwrap();
    assert_eq!(value["kind"], "exit_code_only");
    assert_eq!(value["limitation"], "kvm_exit_code_only");
    assert!(value.get("signal").is_none());
    assert!(value.get("core_dumped").is_none());
}

#[test]
fn harness_capture_separates_guest_streams_from_hermit_diagnostics() {
    let _guard = hermit_run_guard();
    let parent = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let evidence = parent.path().join("evidence");
    let result_path = parent.path().join("result.json");
    let guest_stdout = parent.path().join("guest.stdout");
    let guest_stderr = parent.path().join("guest.stderr");
    let output = run(&[
        "--log",
        "info",
        "run",
        "--run-evidence-dir",
        evidence.to_str().unwrap(),
        "--run-result-json",
        result_path.to_str().unwrap(),
        "--guest-stdout",
        guest_stdout.to_str().unwrap(),
        "--guest-stderr",
        guest_stderr.to_str().unwrap(),
        "--",
        "/bin/sh",
        "-c",
        "printf guest-only-stdout; printf guest-only-stderr >&2; exit 23",
    ]);

    assert_eq!(output.status.code(), Some(23));
    assert!(output.stdout.is_empty());
    assert!(
        !output.stderr.is_empty(),
        "INFO diagnostics were suppressed"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("guest-only-stderr"));
    assert_eq!(fs::read(&guest_stdout).unwrap(), b"guest-only-stdout");
    assert_eq!(fs::read(&guest_stderr).unwrap(), b"guest-only-stderr");

    let result = GuestRunResult::from_current_json_slice(&fs::read(&result_path).unwrap()).unwrap();
    assert_eq!(result.disposition, GuestDisposition::Exited { code: 23 });
    for (stream, path) in [
        (&result.stdout, &guest_stdout),
        (&result.stderr, &guest_stderr),
    ] {
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(stream.bytes, metadata.len());
        assert_eq!(
            stream.sha256,
            detcore::Digest::new(&fs::read(path).unwrap()).to_string()
        );
        assert_eq!(stream.identity.device, metadata.dev());
        assert_eq!(stream.identity.inode, metadata.ino());
    }
    assert_ne!(
        (result.stdout.identity.device, result.stdout.identity.inode),
        (result.stderr.identity.device, result.stderr.identity.inode),
        "guest stdout and stderr must be distinct inodes"
    );
    assert!(matches!(
        inspect_run_evidence(&evidence),
        RunEvidenceInspection::Complete(report)
            if report.outcome == (RunEvidenceOutcome::Complete {
                disposition: GuestDisposition::Exited { code: 23 },
            })
    ));
}
