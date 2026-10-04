/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The root process exits while its child and grandchild are still live; both
//! descendants then observe the reparent and print after the root is gone.
//! Before the exiting task released its descriptors ahead of the terminal
//! boundary receipt, the descendants' EOF wakeups raced Detcore's exit fence
//! and the two actual guests diverged. Three multithreaded writer groups then
//! exercise Detcore's process-retirement fence. In the first, a worker of the
//! root calls exit_group. In the other two, the root exits 0 first, and a
//! worker of its orphaned child calls exit_group or takes a fatal signal. In
//! each, an independent reader prints only after EOF on a pipe that the
//! group's parked peers held. Finally a single-threaded writer, first the root
//! and then an orphan, dies by a synchronous SIGSEGV, a hardware fault rather
//! than an exit that passes through the fence. The run must still report the
//! root's status, 139 when the root itself faults and 0 when the orphan does,
//! and the reader must still print after EOF. The unchanged full INFO/output
//! comparator must match every pair exactly.
//!
//! The exit_group and fatal-signal rows are regression checks, not proof that
//! the fence is needed. A no-fence comparison can still match, so a matching
//! runtime pair alone does not establish the fence's necessity. Detcore's
//! process-retirement unit tests are the binding control for the fence.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use detcore::Digest;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::VerificationReport;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;

pub(super) fn run() {
    let _lock = super::hermit_run_guard();
    // The runner kills a test after 57 seconds. Every row draws on one budget
    // that ends well inside that timeout, so that even when a late row hangs,
    // this test's own bound fails the test and kills the process group instead
    // of racing the runner and orphaning Hermit.
    let deadline = Instant::now() + Duration::from_secs(50);
    let _kvm = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .expect("the selected KVM regression requires /dev/kvm; absence is not a pass");
    let root = tempfile::Builder::new()
        .prefix("kvm-orphan-reparenting-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained reparenting fixture directory")
        .keep();
    eprintln!("KVM reparenting artifacts retained at {}", root.display());
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kvm_orphan_reparenting.c");
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
    assert!(status.success(), "reparenting fixture compilation failed");
    assert!(bounded_read(&compile.join("stdout"), 64 * MIB).is_empty());
    assert!(bounded_read(&compile.join("stderr"), 16 * MIB).is_empty());
    let elf_bytes = bounded_read(&guest, 16 * MIB);
    let elf = goblin::elf::Elf::parse(&elf_bytes).expect("actual reparenting fixture ELF");
    assert_eq!(elf.header.e_type, goblin::elf::header::ET_EXEC);
    assert_eq!(elf.header.e_machine, goblin::elf::header::EM_X86_64);

    let mut completed = 0;
    // Each descendant waits for the root's descriptors to close and then for
    // its own parent to change, so both lines follow the root's in this order.
    // A writer group prints only after every peer and the reader are running,
    // and its reader prints only after EOF, which follows the group's end. A
    // faulting writer prints before its fault, which is what brings EOF.
    let cases: [(&str, i32, &str); 6] = [
        (
            "root-exits",
            7,
            "root: exiting 7 with a live child and grandchild\n\
             late child: parent changed=yes now=other(1)\n\
             late grandchild: parent changed=yes now=other(1)\n",
        ),
        (
            "root-worker-exit-group",
            0,
            "writer: class=root termination=exit_group peers=2\n\
             reader: EOF class=root termination=exit_group peers=2\n",
        ),
        (
            "orphan-worker-exit-group",
            0,
            "writer: class=direct-parent-terminal termination=exit_group peers=2\n\
             reader: EOF class=direct-parent-terminal termination=exit_group peers=2\n",
        ),
        (
            "orphan-worker-fatal",
            0,
            "writer: class=direct-parent-terminal termination=fatal peers=2\n\
             reader: EOF class=direct-parent-terminal termination=fatal peers=2\n",
        ),
        (
            "root-segfault",
            // KVM reports a guest killed by a signal as 128 plus the signal
            // number, with no separate signal.
            139,
            "writer: class=root termination=SIGSEGV\n\
             reader: EOF class=root termination=SIGSEGV\n",
        ),
        (
            "orphan-segfault",
            0,
            "writer: class=direct-parent-terminal termination=SIGSEGV\n\
             reader: EOF class=direct-parent-terminal termination=SIGSEGV\n",
        ),
    ];
    for (mode, expected_status, expected) in cases {
        let directory = root.join(mode);
        let logs = directory.join("verify-logs");
        fs::create_dir_all(&logs).expect("retained verification logs");
        let report_path = directory.join("verification.json");
        let home = directory.join("home");
        let config = directory.join("config");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&config).unwrap();
        let report_arg = report_path.to_str().unwrap();
        let logs_dir = logs.to_str().unwrap();
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
        ];
        if expected_status != 0 {
            // The root's exit status is the observable, so both runs must
            // complete and be compared despite the nonzero status.
            args.push("--verify-allow=failure");
        }
        args.extend([
            "--verify-json",
            report_arg,
            "--keep-logs",
            "--verify-log-dir",
            logs_dir,
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
            "--env=LC_ALL=C",
            "--env=TZ=UTC",
            "--env",
            &home_env,
            "--env",
            &config_env,
            "--",
            guest.to_str().unwrap(),
            mode,
        ]);
        let mut command = super::hermit_command(&args);
        command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
        // A matched pair finishes in about a second; under the previous
        // Reverie pin the first guest never finished.
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "wall budget exhausted before mode {mode}"
        );
        let status = bounded_command_with_timeout(&mut command, &directory, remaining);
        assert_eq!(
            status.code(),
            Some(expected_status),
            "root exit status for mode {mode}"
        );
        assert_eq!(
            bounded_read(&directory.join("stdout"), 64 * MIB),
            expected.as_bytes(),
            "reparenting output for mode {mode}"
        );
        let report = VerificationReport::from_json_slice(&bounded_read(&report_path, 16 * MIB))
            .expect("complete typed verification report");
        report
            .require_canonical_match()
            .expect("full nonempty canonical INFO match");
        report
            .require_exact_output_match()
            .expect("exact two-run status/stdout/stderr match");
        assert_eq!(report.guest_exit_code, Some(expected_status));
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
            assert_eq!(operand.exit_code, Some(expected_status));
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
        completed += 1;
        eprintln!("KVM reparenting mode {mode}: two actual guests and full INFO match");
    }
    assert_eq!(completed, cases.len());
}
