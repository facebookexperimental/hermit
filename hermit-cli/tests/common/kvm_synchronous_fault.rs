// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! CPU-generated SIGSEGV must retain the guest's outcome when the KVM backend
//! retires a root or an orphan whose direct parent has already terminated.

#[path = "hermit_binary.rs"]
mod hermit_binary;

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use detcore::Digest;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::VerificationReport;

use super::kvm_cancellation::RootOutcome;
use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;

pub(super) fn run_root() {
    run("root", RootOutcome::Signal(libc::SIGSEGV), b"");
}

pub(super) fn run_orphan() {
    run("orphan", RootOutcome::Exit(0), b"root saw n=0\n");
}

fn run(mode: &str, expected: RootOutcome, expected_stdout: &[u8]) {
    let _lock = super::hermit_run_guard();
    let _kvm = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .expect("the selected synchronous fault regression requires /dev/kvm");
    let root = tempfile::Builder::new()
        .prefix(&format!("kvm-synchronous-fault-{mode}-"))
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained fixture directory")
        .keep();
    eprintln!(
        "KVM synchronous fault artifacts retained at {}",
        root.display()
    );
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kvm_synchronous_fault.c");
    let source = root.join("guest.c");
    fs::copy(&fixture, &source).expect("retain exact guest instructions");
    assert_eq!(
        bounded_read(&fixture, MIB),
        bounded_read(&source, MIB),
        "compiled guest source is the tracked fixture"
    );
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
            .arg(&source)
            .arg("-o")
            .arg(&guest),
        &compile,
        Duration::from_secs(20),
    );
    assert!(status.success(), "fixture compiler failed");
    assert!(bounded_read(&compile.join("stdout"), 64 * MIB).is_empty());
    assert!(bounded_read(&compile.join("stderr"), 16 * MIB).is_empty());
    let bytes = bounded_read(&guest, 16 * MIB);
    let elf = goblin::elf::Elf::parse(&bytes).expect("actual fixture ELF");
    assert_eq!(elf.header.e_type, goblin::elf::header::ET_EXEC);
    assert_eq!(elf.header.e_machine, goblin::elf::header::EM_X86_64);

    let directory = root.join("run");
    let logs = directory.join("verify-logs");
    fs::create_dir_all(&logs).unwrap();
    let report_path = directory.join("verification.json");
    let home = directory.join("home");
    let config = directory.join("config");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&config).unwrap();
    let home_env = format!("HOME={}", home.display());
    let config_env = format!("XDG_CONFIG_HOME={}", config.display());
    let mut args = vec![
        "--log=info",
        "--backend=kvm",
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
        "--env",
        &home_env,
        "--env",
        &config_env,
    ];
    if expected != RootOutcome::Exit(0) {
        // The root intentionally dies from SIGSEGV; the orphan case must use
        // the default success-only admission gate for the surviving root.
        args.push("--verify-allow=failure");
    }
    args.extend(["--", guest.to_str().unwrap(), mode]);
    let mut command = Command::new(hermit_binary::hermit_binary());
    super::append_hermit_args(&mut command, &args);
    command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
    let status = bounded_command_with_timeout(&mut command, &directory, Duration::from_secs(57));
    // EXIT-CLASS: guest. A synchronous fatal signal in the root ends this CLI
    // with the same signal, as under ptrace, while a fault in an orphan must
    // not fail the live root.
    assert_eq!(
        (status.code(), status.signal()),
        (expected.exit_code(), expected.signal()),
        "exact guest outcome for synchronous {mode} SIGSEGV; artifacts: {}",
        root.display()
    );
    assert_eq!(
        bounded_read(&directory.join("stdout"), 64 * MIB),
        expected_stdout
    );
    let report = VerificationReport::from_json_slice(&bounded_read(&report_path, 16 * MIB))
        .expect("complete typed verification report");
    report
        .require_canonical_match()
        .expect("full nonempty canonical INFO match");
    report
        .require_exact_output_match()
        .expect("exact two-run status/stdout/stderr match");
    assert_eq!(report.guest_exit_code, expected.exit_code());
    assert_eq!(report.guest_signal, expected.signal());
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
    assert_eq!(
        policy.stripped_prefixes.as_deref(),
        Some(["real-wall-clock-prefix/v1".to_owned()].as_slice())
    );
    assert_eq!(
        policy.canonicalizations.as_deref(),
        Some(["host-address-to-first-appearance-ordinal/v1".to_owned()].as_slice())
    );
    let outputs = report.compared_outputs.as_ref().unwrap();
    for operand in [&outputs.left, &outputs.right] {
        assert_eq!(operand.exit_code, expected.exit_code());
        assert_eq!(operand.signal, expected.signal());
        assert_eq!(operand.stdout_bytes, expected_stdout.len() as u64);
        assert_eq!(
            operand.stdout_sha256,
            Digest::new(expected_stdout).to_string()
        );
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
        "one retained golden INFO log of the matched guest"
    );
    assert!(!bounded_read(&golden[0], 64 * MIB).is_empty());
    assert!(
        retained("run2_log_").is_empty(),
        "a matched verification must not retain run 2's log"
    );
    eprintln!("KVM synchronous {mode} SIGSEGV: exact outcomes and full INFO match");
}
