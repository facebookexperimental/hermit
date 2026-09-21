// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! A real caught process timer and a sibling group exit, in both receiver roles.
//! This exercises the complete CLI path without forcing the scheduler-grant to
//! callback-injection interval; deterministic component controls cover that seam.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use detcore::Digest;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::VerificationReport;
use regex::Regex;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;

fn assert_lifecycle(log: &str, expected_status: i32) {
    let lines: Vec<_> = log.lines().collect();
    let signal = Regex::new(r"\[dtid (\d+)\] handling inbound signal \(#0\) SIGALRM").unwrap();
    let deliveries: Vec<_> = signal.captures_iter(log).collect();
    assert_eq!(deliveries.len(), 1, "one real process alarm delivery");
    assert_eq!(log.matches("handling inbound signal").count(), 1);
    let receiver = deliveries[0][1].to_owned();
    let exit = Regex::new(&format!(
        r"\[detcore, dtid (\d+)\] inbound syscall: exit_group\({expected_status}\)"
    ))
    .unwrap();
    let exits: Vec<_> = exit.captures_iter(log).collect();
    assert_eq!(exits.len(), 1, "one real group-exit issuer");
    let issuer = exits[0][1].to_owned();
    assert_ne!(receiver, issuer, "the sibling must issue group exit");
    let delivered = lines.iter().position(|line| signal.is_match(line)).unwrap();
    let requested = lines.iter().position(|line| exit.is_match(line)).unwrap();
    assert!(delivered < requested);
    let commits: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.contains(" COMMIT turn ")
                && line.contains(&format!(
                    "dettid {issuer} using resources {{Exit {{ group: true"
                ))
        })
        .collect();
    assert_eq!(commits.len(), 1, "one winning group-exit commit");
    let commit = commits[0].0;
    assert!(requested < commit);

    let hook =
        Regex::new(r"\[detcore, dtid (\d+)\] thread exit hook, deregistering from scheduler\.")
            .unwrap();
    let hooks: Vec<_> = hook.captures_iter(log).collect();
    assert_eq!(hooks.len(), 2, "both tasks consume exactly one thread hook");
    let actual: BTreeSet<_> = hooks.iter().map(|capture| capture[1].to_owned()).collect();
    assert_eq!(actual, BTreeSet::from([receiver.clone(), issuer.clone()]));
    let mut last_deregistered = commit;
    for tid in [&receiver, &issuer] {
        let start =
            format!("[detcore, dtid {tid}] thread exit hook, deregistering from scheduler.");
        let done =
            format!("[detcore, dtid {tid}] thread deregistered, removed from sched structures.");
        let starts: Vec<_> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains(&start))
            .collect();
        let dones: Vec<_> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains(&done))
            .collect();
        assert_eq!(starts.len(), 1);
        assert_eq!(dones.len(), 1);
        assert!(commit < starts[0].0 && starts[0].0 < dones[0].0);
        last_deregistered = last_deregistered.max(dones[0].0);
    }
    let cleanups: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("Global state cleanup, continuing..."))
        .collect();
    assert_eq!(cleanups.len(), 1);
    assert!(last_deregistered < cleanups[0].0);
    assert_eq!(
        log.matches("detcore shut down, destroying global state")
            .count(),
        1
    );
    // The process Tool hook is silent; the component controls check its exact
    // count. Global cleanup is an independently visible end-to-end boundary.
}

pub(super) fn run() {
    let _lock = super::hermit_run_guard();
    let _kvm = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .expect("the selected retirement regression requires /dev/kvm");
    fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).expect("fixture parent");
    let root = tempfile::Builder::new()
        .prefix("kvm-signal-retirement-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained fixture directory")
        .keep();
    eprintln!(
        "KVM signal retirement artifacts retained at {}",
        root.display()
    );
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kvm_signal_retirement.c");
    fs::copy(&fixture, root.join("guest.c")).expect("retain exact guest instructions");
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
    assert!(status.success(), "fixture compiler failed");
    assert!(bounded_read(&compile.join("stdout"), 64 * MIB).is_empty());
    assert!(bounded_read(&compile.join("stderr"), 16 * MIB).is_empty());
    let bytes = bounded_read(&guest, 16 * MIB);
    let elf = goblin::elf::Elf::parse(&bytes).expect("actual fixture ELF");
    assert_eq!(elf.header.e_type, goblin::elf::header::ET_EXEC);
    assert_eq!(elf.header.e_machine, goblin::elf::header::EM_X86_64);

    let mut completed = 0;
    for (mode, receiver, expected_status) in [("0", "leader", 95), ("1", "worker", 17)] {
        let directory = root.join(mode);
        let logs = directory.join("verify-logs");
        fs::create_dir_all(&logs).unwrap();
        let report_path = directory.join("verification.json");
        let home = directory.join("home");
        let config = directory.join("config");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&config).unwrap();
        let args = [
            "--log=trace",
            "run",
            "--base-env=minimal",
            "--backend=kvm",
            "--strict",
            "--target-timeslice=6000",
            "--verify-strict",
            "--verify",
            "--verify-allow=failure",
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
            mode,
        ];
        let mut command = super::hermit_command(&args);
        command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
        let status =
            bounded_command_with_timeout(&mut command, &directory, Duration::from_secs(57));
        assert_eq!(
            status.code(),
            Some(expected_status),
            "exact winning exit status for mode {mode}"
        );
        let expected = format!("timer-retirement: {receiver} handler\n");
        assert_eq!(
            bounded_read(&directory.join("stdout"), 64 * MIB),
            expected.as_bytes()
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
                .map(|e| e.unwrap().path())
                .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
                .collect();
            assert_eq!(matches.len(), 1, "one retained log per actual guest");
            let log =
                String::from_utf8(bounded_read(&matches[0], 64 * MIB)).expect("complete trace");
            assert_lifecycle(&log, expected_status);
        }
        completed += 1;
        eprintln!("KVM signal retirement mode {mode}: two positive guests and full INFO match");
    }
    assert_eq!(completed, 2);
}
