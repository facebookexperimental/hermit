// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! Positive ITIMER_REAL delivery while sleeping, including worker arm/exit.
//! One selected declaration runs eight modes, each with two actual guests and
//! the unchanged full INFO/output comparator. The enclosing official runner
//! owns aggregate CPU containment; the shared helper retains wall/output bounds.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use detcore::Digest;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::VerificationReport;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;

pub(super) fn run() {
    let _lock = super::hermit_run_guard();
    let _kvm = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .expect("the selected KVM regression requires /dev/kvm; absence is not a pass");
    let root = tempfile::Builder::new()
        .prefix("kvm-itimer-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained timer fixture directory")
        .keep();
    eprintln!("KVM timer artifacts retained at {}", root.display());
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/kvm_itimer_sleep_continuation.c");
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
                "-pthread",
            ])
            .arg(&fixture)
            .arg("-o")
            .arg(&guest),
        &compile,
        Duration::from_secs(20),
    );
    assert!(status.success(), "timer fixture compilation failed");
    assert!(bounded_read(&compile.join("stdout"), 64 * MIB).is_empty());
    assert!(bounded_read(&compile.join("stderr"), 16 * MIB).is_empty());
    let elf_bytes = bounded_read(&guest, 16 * MIB);
    let elf = goblin::elf::Elf::parse(&elf_bytes).expect("actual timer fixture ELF");
    assert_eq!(elf.header.e_type, goblin::elf::header::ET_EXEC);
    assert_eq!(elf.header.e_machine, goblin::elf::header::EM_X86_64);

    let mut completed_modes = 0;
    for restart in ["0", "1"] {
        for absolute in ["0", "1"] {
            for worker in ["0", "1"] {
                let mode = format!("{restart}{absolute}{worker}");
                let directory = root.join(&mode);
                let logs = directory.join("verify-logs");
                fs::create_dir_all(&logs).expect("retained verification logs");
                let report_path = directory.join("verification.json");
                let home = directory.join("home");
                let config = directory.join("config");
                fs::create_dir_all(&home).unwrap();
                fs::create_dir_all(&config).unwrap();
                let args = [
                    "--log=info",
                    "run",
                    "--base-env=minimal",
                    "--backend=kvm",
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
                    &format!("HOME={}", home.display()),
                    "--env",
                    &format!("XDG_CONFIG_HOME={}", config.display()),
                    "--",
                    guest.to_str().unwrap(),
                    restart,
                    absolute,
                    worker,
                ];
                let mut command = super::hermit_command(&args);
                command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
                let status =
                    bounded_command_with_timeout(&mut command, &directory, Duration::from_secs(57));
                assert_eq!(status.code(), Some(0), "actual timer mode {mode} status");
                // The fixture emits this only after checking one SI_KERNEL
                // delivery to the leader, EINTR with either SA_RESTART setting,
                // relative remaining time or an untouched absolute sentinel,
                // disarm state and a successful zero-duration sleep.
                let expected = format!(
                    "PASS caught=1 code=SI_KERNEL receiver=leader restart={restart} absolute={absolute} worker_arm_exit={worker}\n"
                );
                assert_eq!(
                    bounded_read(&directory.join("stdout"), 64 * MIB),
                    expected.as_bytes(),
                    "positive timer output for mode {mode}"
                );
                let report =
                    VerificationReport::from_json_slice(&bounded_read(&report_path, 16 * MIB))
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
                    assert_eq!(operand.exit_code, Some(0));
                    assert!(operand.signal.is_none());
                    assert_eq!(operand.stdout_bytes, expected.len() as u64);
                    assert_eq!(
                        operand.stdout_sha256,
                        Digest::new(expected.as_bytes()).to_string()
                    );
                    assert_eq!(operand.stderr_bytes, 0);
                    assert_eq!(operand.stderr_sha256, Digest::new(b"").to_string());
                }
                for prefix in ["run1_log_", "run2_log_"] {
                    let matches: Vec<_> = fs::read_dir(&logs)
                        .unwrap()
                        .map(|entry| entry.unwrap().path())
                        .filter(|path| {
                            path.file_name()
                                .unwrap()
                                .to_string_lossy()
                                .starts_with(prefix)
                        })
                        .collect();
                    assert_eq!(matches.len(), 1, "one retained log per actual guest");
                    assert!(!bounded_read(&matches[0], 64 * MIB).is_empty());
                }
                completed_modes += 1;
                eprintln!("KVM timer mode {mode}: two positive guests and full INFO match");
            }
        }
    }
    assert_eq!(completed_modes, 8);
}
