// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! Exercise raw waitid through the real KVM CLI and Detcore. The guest emits
//! complete caller arenas because the generic waitid IO hook does not observe
//! every error write, padding byte, or overlapping output. Exact stdout plus
//! the full canonical INFO comparison covers these specific observations.
//! Immediate logical-child and CPU accounting invariants have component tests;
//! eventual guest ECHILD by itself would not establish their ordering.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use detcore::Digest;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::RecordEnvelopeReport;
use hermit::canonical_verdict::VerificationReport;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;
use super::kvm_cancellation::bounded_verify_command;

const MIB: u64 = 1024 * 1024;

const TERMINAL_CASES: &[&str] = &[
    "writable",
    "null-info",
    "null-usage",
    "both-null",
    "protected-unused-tail",
    "read-only-info",
    "inaccessible-info",
    "read-only-usage",
    "inaccessible-usage",
    "usage-prefix-1",
    "usage-prefix-8",
    "usage-prefix-64",
    "first-scalar-split",
    "pid-scalar-split",
    "status-scalar-split",
    "aliased-outputs",
    "wnowait-writable",
    "wnowait-info-fault",
    "wnowait-usage-fault",
    "high-option-register-bits",
    "p-all-ignores-id",
    "fault-preserves-sibling",
];

const ERROR_CASES: &[&str] = &[
    "already-reaped",
    "invalid-options",
    "invalid-options-null",
    "invalid-options-protected",
    "pid-zero",
    "pid-zero-null",
    "pid-zero-protected",
    "pid-negative",
    "pgid-negative",
    "invalid-selector",
    "live-no-event",
    "live-no-event-null",
    "live-no-event-protected",
    "live-usage-protected",
    "live-high-options",
    "live-p-all-ignores-id",
    "interrupted-protected-info",
    "interrupted-writable-info",
    "restarted-writable-info",
    "ignored-sibling-sigurg",
];

fn assert_observations(stdout: &[u8], mode: &str, expected: &[&str], children: u64) {
    let text = std::str::from_utf8(stdout).expect("complete fixture UTF-8");
    assert!(text.ends_with('\n'));
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("one complete JSON observation per line"))
        .collect();
    let mut names = BTreeSet::new();
    let mut checks = 0;
    let mut summaries = 0;
    let mut ignored_child = None;
    let mut ignored_checks = 0;
    for (index, row) in rows.iter().enumerate() {
        match row["type"].as_str().expect("observation type") {
            "case" => {
                let name = row["name"].as_str().expect("case name");
                assert!(names.insert(name), "duplicate case {name}");
                for (before, after) in [("info_before", "info_after"), ("aux_before", "aux_after")]
                {
                    let before = row[before].as_str().expect("entire before arena");
                    let after = row[after].as_str().expect("entire after arena");
                    assert!(!before.is_empty() && before.len() <= 4 * 65536);
                    assert_eq!(before.len(), after.len());
                    assert_eq!(before.len() % 2, 0);
                    assert!(
                        before
                            .as_bytes()
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .all(|pair| pair == b"a5")
                    );
                    assert!(after.bytes().all(|byte| byte.is_ascii_hexdigit()));
                }
                assert!(row["rc"].is_i64());
                assert!(row["errno"].is_i64());
                if name == "ignored-sibling-sigurg" {
                    assert!(ignored_child.replace(row["id"].as_u64().unwrap()).is_none());
                    for (key, expected) in [
                        ("which", 1),
                        ("rc", -1),
                        ("errno", libc::EFAULT),
                        ("alarms", 1),
                        ("info_mode", 2),
                        ("info_offset", 64),
                        ("usage_protection", 3),
                        ("null_usage", 1),
                        ("signal", libc::SIGURG),
                        ("wait_calls", 1),
                        ("send_calls", 1),
                        ("send_rc", 0),
                        ("send_errno", 0),
                        ("pre_returned", 0),
                        ("release_seen", 1),
                        ("release_rc", 1),
                        ("release_errno", 0),
                        ("ack_rc", 1),
                        ("ack_errno", 0),
                        ("sender_failure", 0),
                    ] {
                        assert_eq!(row[key].as_i64(), Some(i64::from(expected)), "{key}");
                    }
                    assert_eq!(row["options"], "0x4");
                    assert_eq!(row["signal_default"], true);
                    for key in ["id", "tgid", "waiter_tid", "sender_tid"] {
                        assert!(row[key].as_i64().unwrap() > 0, "{key}");
                    }
                    assert_ne!(row["waiter_tid"], row["sender_tid"]);
                    assert_eq!(row["tgid"], row["waiter_tid"]);
                    assert!(row["release_fd"].as_i64().unwrap() >= 0);
                    assert_eq!(row["pre_info"], row["info_before"]);
                    assert_eq!(row["pre_aux"], row["aux_before"]);
                    assert_eq!(row["info_after"], row["info_before"]);
                    assert_eq!(row["aux_after"], row["aux_before"]);
                }
                if matches!(
                    name,
                    "interrupted-writable-info" | "restarted-writable-info"
                ) {
                    let before = row["info_before"].as_str().unwrap();
                    let mut zero_fields = before.as_bytes().to_vec();
                    for offset in [0, 4, 8, 16, 20, 24] {
                        zero_fields[2 * (64 + offset)..2 * (68 + offset)].fill(b'0');
                    }
                    assert_eq!(row["alarms"], 1);
                    assert_eq!(row["aux_after"], row["aux_before"]);
                    if name == "interrupted-writable-info" {
                        assert_eq!(row["rc"], -1);
                        assert_eq!(row["errno"], libc::EINTR);
                        assert_eq!(
                            row["info_after"].as_str().unwrap().as_bytes(),
                            zero_fields.as_slice()
                        );
                    } else {
                        assert_eq!(row["rc"], 0);
                        assert_eq!(row["errno"], 0);
                        assert_eq!(row["null_usage"], 1);
                        assert_eq!(row["info_offset"], 64);
                        assert_eq!(row["sa_restart"], true);
                        assert_eq!(row["wait_calls"], 1);
                        assert_eq!(row["handler_seen"], 1);
                        assert_eq!(row["release_rc"], 1);
                        assert_eq!(row["release_errno"], 0);
                        assert_eq!(
                            row["handler_info"].as_str().unwrap().as_bytes(),
                            zero_fields.as_slice()
                        );
                    }
                }
            }
            "check" => {
                checks += 1;
                let arena = row["arena"].as_str().expect("full guarded follow-up arena");
                assert_eq!(arena.len(), 320);
                assert!(arena.bytes().all(|byte| byte.is_ascii_hexdigit()));
                if row["name"] == "ignored-child-ECHILD" {
                    ignored_checks += 1;
                    assert_eq!(row["pid"].as_u64(), ignored_child);
                    assert_eq!(row["rc"], -1);
                    assert_eq!(row["errno"], libc::ECHILD);
                    assert_eq!(row["options"], "0x5");
                    let mut expected = "a5".repeat(160).into_bytes();
                    for offset in [0, 4, 8, 16, 20, 24] {
                        expected[2 * (16 + offset)..2 * (20 + offset)].fill(b'0');
                    }
                    assert_eq!(arena.as_bytes(), expected.as_slice());
                }
            }
            "retained" => {
                assert_eq!(row["rc"], 0);
                assert_eq!(row["errno"], 0);
            }
            "summary" => {
                summaries += 1;
                assert_eq!(
                    index + 1,
                    rows.len(),
                    "summary must be the final observation"
                );
                assert_eq!(row["mode"], mode);
                assert_eq!(row["cases"].as_u64(), Some(expected.len() as u64));
                assert_eq!(row["children"].as_u64(), Some(children));
                assert_eq!(row["passed"], true);
                assert!(row["assertions"].as_u64().unwrap() > expected.len() as u64);
            }
            other => panic!("unexpected observation {other}"),
        }
    }
    let expected_names: BTreeSet<_> = expected.iter().copied().collect();
    assert_eq!(names, expected_names);
    assert!(
        checks > 0,
        "actual child peeks and reap checks must execute"
    );
    assert_eq!(summaries, 1);
    assert_eq!(ignored_checks, usize::from(mode == "errors"));
}

// Full INFO records can span lines (scheduler commits do). Retain their
// boundaries; do not treat a bare SIGURG substring as a signal grant.
fn info_records(text: &str) -> Vec<String> {
    assert!(text.ends_with('\n'), "complete retained INFO log");
    let header = regex::Regex::new(r"^\S+\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+\S+: (.*)$").unwrap();
    let mut records = Vec::new();
    let mut current: Option<(bool, String)> = None;
    for line in text.lines() {
        if let Some(capture) = header.captures(line) {
            if let Some((true, message)) = current.take() {
                records.push(message.trim_end_matches('\n').to_owned());
            }
            current = Some((&capture[1] == "INFO", capture[2].to_owned()));
        } else if let Some((_, message)) = current.as_mut() {
            message.push('\n');
            message.push_str(line);
        }
    }
    if let Some((true, message)) = current {
        records.push(message.trim_end_matches('\n').to_owned());
    }
    assert!(!records.is_empty());
    records
}

fn assert_ignored_signal_trace(log: &[u8], stdout: &[u8]) {
    let rows: Vec<serde_json::Value> = std::str::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let selected: Vec<_> = rows
        .iter()
        .filter(|row| row["name"] == "ignored-sibling-sigurg")
        .collect();
    assert_eq!(selected.len(), 1);
    let row = selected[0];
    let waiter = row["waiter_tid"].as_u64().unwrap();
    let sender = row["sender_tid"].as_u64().unwrap();
    let child = row["id"].as_u64().unwrap();
    let tgid = row["tgid"].as_u64().unwrap();
    let fd = row["release_fd"].as_u64().unwrap();
    assert!(waiter > 0 && sender > 0 && child > 0 && tgid > 0 && waiter != sender);
    let records = info_records(std::str::from_utf8(log).expect("complete UTF-8 INFO log"));
    let entry = regex::Regex::new(
        r"^DETLOG \[syscall\]\[detcore, dtid (\d+)\] inbound syscall: (.+) = \?$",
    )
    .unwrap();
    let finish = regex::Regex::new(
        r"^DETLOG \[syscall\]\[detcore, dtid (\d+)\] finish syscall #(\d+): (.+) = (.+)$",
    )
    .unwrap();
    let mut entries = Vec::new();
    let mut results = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let Some((message, metadata)) = record.rsplit_once(" DETLOG_RECORD=") else {
            continue;
        };
        if let Some(capture) = entry.captures(message) {
            let value: serde_json::Value = serde_json::from_str(metadata).unwrap();
            assert_eq!(value["schema"], 1);
            assert_eq!(value["event"]["kind"], "syscall");
            entries.push((
                index,
                capture[1].parse::<u64>().unwrap(),
                capture[2].to_owned(),
            ));
        } else if let Some(capture) = finish.captures(message) {
            let value: serde_json::Value = serde_json::from_str(metadata).unwrap();
            assert_eq!(value["schema"], 1);
            assert_eq!(value["event"]["kind"], "syscall_result");
            assert_eq!(
                value["event"]["finished_syscall_number"].as_u64(),
                Some(capture[2].parse::<u64>().unwrap())
            );
            results.push((
                index,
                capture[1].parse::<u64>().unwrap(),
                capture[3].to_owned(),
                capture[4].to_owned(),
            ));
        }
    }
    let pair = |tid: u64, pattern: &str, result: &str| {
        let expression = regex::Regex::new(pattern).unwrap();
        let starts: Vec<_> = entries
            .iter()
            .filter(|(_, actual, call)| *actual == tid && expression.is_match(call))
            .collect();
        assert_eq!(starts.len(), 1, "one exact syscall entry: {pattern}");
        let (begin, _, call) = starts[0];
        let ends: Vec<_> = results
            .iter()
            .filter(|(_, actual, actual_call, _)| *actual == tid && actual_call == call)
            .collect();
        assert_eq!(ends.len(), 1, "one exact syscall result: {call}");
        assert_eq!(ends[0].3, result);
        assert!(*begin < ends[0].0);
        (*begin, ends[0].0)
    };
    let wait = pair(
        waiter,
        &format!(r"^waitid\(1, {child}, 0x[0-9a-f]+, 4, NULL\)$"),
        "Err(Errno(EFAULT))",
    );
    let send = pair(
        sender,
        &format!(r"^tgkill\({tgid}, {waiter}, 23\)$"),
        "Ok(0)",
    );
    let release = pair(
        sender,
        &format!(r"^write\({fd}, 0x[0-9a-f]+, 1\)$"),
        "Ok(1)",
    );
    let gone = pair(
        waiter,
        &format!(r"^waitid\(1, {child}, 0x[0-9a-f]+, 5, NULL\)$"),
        "Err(Errno(ECHILD))",
    );
    let park = regex::Regex::new(&format!(r"^\[scheduler\] NONCOMMIT turn \d+, parking dettid {waiter} for child ChildWaitSpec \{{ selector: Exact\(DetPid\({child}\)\), owner: None, exit_class: Sigchld \}}$")).unwrap();
    // This single-resource Debug spelling is bound to SigWrapper's actual
    // derive and scheduler formatter. Until a released run emits it, the
    // proposal makes no claim to have observed this particular event.
    let grant = regex::Regex::new(&format!(r"(?s)^\[sched-step5\] >>>>>>>\n\n COMMIT turn (\d+), dettid {waiter} using resources \{{WaitidSignals\(\[SigWrapper\(23\)\]\): W\}}, on previously committed [^\n]+$")).unwrap();
    let mut grants = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let Some((message, metadata)) = record.rsplit_once(" DETLOG_RECORD=") else {
            continue;
        };
        if let Some(capture) = grant.captures(message) {
            let value: serde_json::Value = serde_json::from_str(metadata).unwrap();
            assert_eq!(value["schema"], 1);
            assert_eq!(value["event"]["kind"], "scheduler_commit");
            assert_eq!(
                value["event"]["scheduler_turn"].as_u64(),
                Some(capture[1].parse::<u64>().unwrap())
            );
            grants.push(index);
        }
    }
    assert_eq!(grants.len(), 1, "one actual ignored-signal grant");
    let granted = grants[0];
    assert!(wait.0 < send.0 && send.0 < granted && send.1 < release.0);
    assert!(granted < release.0 && release.0 < wait.1 && wait.1 < gone.0);
    assert!(
        records
            .iter()
            .enumerate()
            .any(|(n, message)| wait.0 < n && n < send.0 && park.is_match(message)),
        "actual initial park before sibling send"
    );
    assert!(
        records
            .iter()
            .enumerate()
            .any(|(n, message)| granted < n && n < release.0 && park.is_match(message)),
        "actual re-park before child release"
    );
}

fn assert_sibling_observations(stdout: &[u8], direction: &str, selector: &str, consume: &str) {
    let text = std::str::from_utf8(stdout).expect("complete sibling fixture UTF-8");
    assert!(text.ends_with('\n'));
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("complete sibling JSON observation"))
        .collect();
    let fault = consume == "efault";
    assert_eq!(rows.len(), if fault { 11 } else { 10 });
    let summary = rows.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["direction"], direction);
    assert_eq!(summary["selector"], selector);
    assert_eq!(summary["consume"], consume);
    assert_eq!(summary["calls"], 9);
    assert_eq!(summary["passed"], true);
    let leader = summary["leader"].as_u64().unwrap();
    let worker = summary["worker"].as_u64().unwrap();
    let child = summary["child"].as_u64().unwrap();
    assert!(leader > 0 && worker > 0 && child > 0);
    assert!(leader != worker && leader != child && worker != child);
    let (creator, waiter) = if direction == "leader-child" {
        (leader, worker)
    } else {
        (worker, leader)
    };
    let calls: Vec<_> = rows[..rows.len() - 1]
        .iter()
        .filter(|row| row["type"] == "wait")
        .collect();
    assert_eq!(calls.len(), 9);
    let names = [
        if direction == "leader-child" {
            "creator leader proves readiness before pthread_create"
        } else {
            "creator worker proves child waitability"
        },
        "sibling owner restriction leaves child intact",
        "first sibling peek values and padding",
        "repeated sibling peek values and padding",
        if fault {
            "usage fault precedes every info store"
        } else {
            "sibling consumption values and padding"
        },
        "exact child consumed once",
        "all children consumed once",
        "exact child consumed once",
        "all children consumed once",
    ];
    let flags = [
        0x1000004_u64,
        0x21000004,
        0x1000004,
        0x1000004,
        4,
        5,
        5,
        5,
        5,
    ];
    let uid = u32::try_from(calls[0]["uid"].as_u64().unwrap()).unwrap();
    for (index, row) in calls.iter().enumerate() {
        assert_eq!(row["name"], names[index]);
        assert_eq!(row["child"].as_u64(), Some(child));
        assert_eq!(row["uid"].as_u64(), Some(u64::from(uid)));
        assert_eq!(row["options"].as_u64(), Some(flags[index]));
        let owner_call = index == 0 || index >= 7;
        assert_eq!(
            row["tid"].as_u64(),
            Some(if owner_call { creator } else { waiter })
        );
        let all = match index {
            0 | 5 | 7 => false,
            6 | 8 => true,
            _ => selector == "all",
        };
        assert_eq!(row["which"], if all { 0 } else { 1 });
        let error = if index == 1 || index >= 5 {
            libc::ECHILD
        } else if index == 4 && fault {
            libc::EFAULT
        } else {
            0
        };
        assert_eq!(row["errno"], error);
        assert_eq!(row["rc"], if error == 0 { 0 } else { -1 });
        assert_eq!(row["before"], "a5".repeat(160));
        let mut expected = [0xa5_u8; 160];
        if !(index == 4 && fault) {
            let fields = if error == 0 {
                [
                    libc::SIGCHLD as u32,
                    0,
                    libc::CLD_EXITED as u32,
                    u32::try_from(child).unwrap(),
                    uid,
                    73,
                ]
            } else {
                [0; 6]
            };
            for (offset, value) in [0, 4, 8, 16, 20, 24].into_iter().zip(fields) {
                expected[16 + offset..20 + offset].copy_from_slice(&value.to_le_bytes());
            }
        }
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(320);
        for byte in expected {
            encoded.push(HEX[usize::from(byte >> 4)] as char);
            encoded.push(HEX[usize::from(byte & 15)] as char);
        }
        assert_eq!(row["after"], encoded, "entire caller arena at row {index}");
    }
    if fault {
        let usage = &rows[5];
        assert_eq!(usage["type"], "usage");
        let before = usage["before"].as_str().unwrap();
        assert!((8192..=131072).contains(&before.len()));
        assert_eq!(before.len() % 2, 0);
        assert!(
            before
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .all(|pair| pair == b"a5")
        );
        assert_eq!(usage["after"], before);
        assert!(
            rows[..5]
                .iter()
                .chain(rows[6..10].iter())
                .all(|row| row["type"] == "wait")
        );
    } else {
        assert!(rows[..9].iter().all(|row| row["type"] == "wait"));
    }
}

pub(super) fn run(mode: &str) {
    let (expected, children) = match mode {
        "terminal" => (TERMINAL_CASES, 23),
        "errors" => (ERROR_CASES, 5),
        _ => panic!("unknown waitid fixture mode"),
    };
    run_fixture(
        mode,
        "kvm_waitid_copyout.c",
        &[mode],
        |stdout| assert_observations(stdout, mode, expected, children),
        |full_log, stdout| {
            if mode == "errors" {
                assert_ignored_signal_trace(full_log, stdout);
            }
        },
    );
}

pub(super) fn run_sibling(direction: &str, selector: &str, consume: &str) {
    assert!(matches!(direction, "leader-child" | "worker-child"));
    assert!(matches!(selector, "pid" | "all"));
    assert!(matches!(consume, "success" | "efault"));
    let mode = format!("sibling-{direction}-{selector}-{consume}");
    run_fixture(
        &mode,
        "kvm_waitid_sibling.c",
        &[direction, selector, consume],
        |stdout| assert_sibling_observations(stdout, direction, selector, consume),
        |_, _| {},
    );
}

// Shared compile, resource bounds and complete typed verification. The callers
// retain their own additional guest assertions; none replaces the policy below.
fn run_fixture(
    mode: &str,
    fixture_name: &str,
    guest_args: &[&str],
    check_stdout: impl Fn(&[u8]),
    check_log: impl Fn(&[u8], &[u8]),
) {
    let _lock = super::hermit_run_guard();
    // One shared deadline includes fixture compilation and the two-guest
    // verification, inside the existing 57-second outer test timeout.
    let deadline = Instant::now() + Duration::from_secs(50);
    let _kvm = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .expect("selected waitid regression requires /dev/kvm; absence is not a pass");
    fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).expect("fixture parent");
    let root = tempfile::Builder::new()
        .prefix("kvm-waitid-copyout-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained fixture directory")
        .keep();
    eprintln!(
        "KVM waitid copyout artifacts retained at {}",
        root.display()
    );
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture_name);
    fs::copy(&fixture, root.join("guest.c")).expect("retain exact fixture source");
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
    assert!(status.success(), "waitid fixture compilation failed");
    assert!(bounded_read(&compile.join("stdout"), 64 * MIB).is_empty());
    assert!(bounded_read(&compile.join("stderr"), 16 * MIB).is_empty());
    let bytes = bounded_read(&guest, 16 * MIB);
    let elf = goblin::elf::Elf::parse(&bytes).expect("actual fixture ELF");
    assert_eq!(elf.header.e_type, goblin::elf::header::ET_EXEC);
    assert_eq!(elf.header.e_machine, goblin::elf::header::EM_X86_64);

    let directory = root.join(mode);
    let logs = directory.join("verify-logs");
    fs::create_dir_all(&logs).unwrap();
    let report_path = directory.join("verification.json");
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
        "--",
        guest.to_str().unwrap(),
    ];
    args.extend_from_slice(guest_args);
    let mut command = super::hermit_command(&args);
    command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .expect("test deadline");
    let (status, run2_log) = bounded_verify_command(&mut command, &directory, remaining);
    assert_eq!(status.code(), Some(0), "both guests must actually succeed");
    let stdout = bounded_read(&directory.join("stdout"), 64 * MIB);
    check_stdout(&stdout);
    let report = VerificationReport::from_current_json_slice(&bounded_read(&report_path, 16 * MIB))
        .expect("complete current typed verification report");
    report
        .require_canonical_match()
        .expect("full nonempty canonical INFO match");
    report
        .require_exact_output_match()
        .expect("exact output/status parity");
    assert_eq!(report.guest_exit_code, Some(0));
    assert!(report.guest_signal.is_none());
    let counts = report.compared_log_messages.unwrap();
    assert!(counts.left > 0);
    assert_eq!(counts.left, counts.right);
    let policy = report.comparison.as_ref().unwrap();
    assert_eq!(policy.display_name.as_deref(), Some("BitwiseInfoV1"));
    assert!(policy.compare_logs);
    assert_eq!(policy.record_envelope, RecordEnvelopeReport::AllRecordsV1);
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
        assert_eq!(operand.stdout_bytes, stdout.len() as u64);
        assert_eq!(operand.stdout_sha256, Digest::new(&stdout).to_string());
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
    // After a match `--keep-logs` keeps only run 1's log, the golden copy, and
    // deletes run 2's, which matched it. Run 2's log was hard-linked while the
    // command ran; that capture gets the same full-log checks as the golden log.
    let golden = retained("run1_log_");
    assert_eq!(
        golden.len(),
        1,
        "one retained golden full log of the matched guest"
    );
    assert!(
        retained("run2_log_").is_empty(),
        "a matched verification must not retain run 2's log"
    );
    let run2_log = run2_log.expect("run 2's log, captured while the command ran");
    for path in [&golden[0], &run2_log] {
        let full_log = bounded_read(path, 64 * MIB);
        assert!(!full_log.is_empty());
        check_log(&full_log, &stdout);
    }
    fs::remove_file(&run2_log).expect("remove run 2's checked log");
    assert!(
        Instant::now() < deadline,
        "complete test stays inside its shared bound"
    );
}

// Keep the mixed wait4/waitid contract under the same complete INFO and I/O
// verification as the waitid fixtures above. Raw wait4 rusage is not compared
// with native CPU time: children accounting is observed through getrusage.
pub(super) fn run_wait4_fault() {
    run_fixture(
        "wait4-consuming-fault",
        "kvm_wait4_fault.c",
        &[],
        assert_wait4_fault_observations,
        |_, _| {},
    );
}

fn assert_wait4_fault_observations(stdout: &[u8]) {
    let rows: Vec<serde_json::Value> = std::str::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("complete mixed-wait observation"))
        .collect();
    assert_eq!(rows.len(), 8 * 19 + 1);
    let names = [
        "target-peek",
        "before-fault",
        "consuming-fault",
        "after-fault",
        "target-ECHILD",
        "target-wait4-ECHILD",
        "live-sibling-P_ALL",
        "live-sibling-wait4-P_ALL",
        "after-empty-waits",
        "sibling-peek",
        "sibling-peek-again",
        "after-sibling-peeks",
        "sibling-consume",
        "after-sibling-consume",
        "sibling-ECHILD",
        "all-ECHILD",
        "all-wait4-ECHILD",
        "after-final-ECHILD",
    ];
    let encode = |bytes: &[u8]| {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        bytes
            .iter()
            .flat_map(|byte| {
                [
                    DIGITS[(byte >> 4) as usize] as char,
                    DIGITS[(byte & 15) as usize] as char,
                ]
            })
            .collect::<String>()
    };
    let mut children = BTreeSet::new();
    for case in 0..8 {
        let observations = &rows[case * 19..(case + 1) * 19];
        let summary = &observations[18];
        let fault = case / 4 + 1;
        let nonblock = (case / 2) % 2;
        let untraced = case % 2;
        assert_eq!(summary["type"], "case");
        assert_eq!(summary["case"], case);
        assert_eq!(summary["fault"], fault);
        assert_eq!(summary["nonblock"], nonblock);
        assert_eq!(summary["untraced"], untraced);
        assert_eq!(summary["passed"], true);
        let child = summary["child"].as_i64().unwrap();
        let sibling = summary["sibling"].as_i64().unwrap();
        assert!(child > 0 && sibling > 0 && child != sibling);
        assert!(children.insert(child) && children.insert(sibling));
        let mut cpus = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let row = &observations[index];
            assert_eq!(row["case"], case);
            assert_eq!(row["name"], *name);
            match index {
                1 | 3 | 8 | 11 | 13 | 17 => {
                    assert_eq!(row["type"], "cpu");
                    let cpu = (
                        row["user_us"].as_i64().unwrap(),
                        row["system_us"].as_i64().unwrap(),
                    );
                    assert!(cpu.0 >= 0 && cpu.1 >= 0);
                    cpus.push(cpu);
                }
                2 | 5 | 7 | 16 => {
                    assert_eq!(row["type"], "wait4");
                    let consumes = index == 2;
                    let selector = if index == 5 || (consumes && untraced == 0) {
                        child
                    } else {
                        -1
                    };
                    assert_eq!(row["selector"], selector);
                    assert_eq!(
                        row["options"],
                        if consumes {
                            nonblock | (untraced << 1)
                        } else {
                            1
                        }
                    );
                    assert_eq!(row["fault"], if consumes { fault } else { 0 });
                    assert_eq!(row["status"], if consumes { 31 + case } else { 0 });
                    assert_eq!(row["rc"], if index == 7 { 0 } else { -1 });
                    assert_eq!(
                        row["errno"],
                        if consumes {
                            libc::EFAULT
                        } else if index == 7 {
                            0
                        } else {
                            libc::ECHILD
                        }
                    );
                    let mut status = [0xa5; 4096];
                    if consumes && fault == 2 {
                        status[64..68].copy_from_slice(&(((31 + case) as i32) << 8).to_le_bytes());
                    }
                    assert_eq!(row["status_arena"], encode(&status));
                    assert_eq!(row["usage_arena"], "a5".repeat(4096));
                }
                _ => {
                    assert_eq!(row["type"], "waitid");
                    let event = matches!(index, 0 | 9 | 10 | 12);
                    let all = matches!(index, 6 | 15);
                    let id = if index == 15 {
                        0
                    } else if matches!(index, 0 | 4) {
                        child
                    } else {
                        sibling
                    };
                    let status = if !event {
                        0
                    } else if index == 0 {
                        31 + case
                    } else {
                        73 + case
                    };
                    let options = if matches!(index, 0 | 9 | 10) {
                        libc::WEXITED | libc::WNOWAIT
                    } else if index == 12 {
                        libc::WEXITED
                    } else {
                        libc::WEXITED | libc::WNOHANG
                    };
                    let error = if matches!(index, 4 | 14 | 15) {
                        libc::ECHILD
                    } else {
                        0
                    };
                    assert_eq!(row["child"], id);
                    assert_eq!(row["which"], if all { libc::P_ALL } else { libc::P_PID });
                    assert_eq!(row["options"], options);
                    assert_eq!(row["status"], status);
                    assert_eq!(row["event"], i32::from(event));
                    assert_eq!(row["rc"], if error == 0 { 0 } else { -1 });
                    assert_eq!(row["errno"], error);
                    let uid = u32::try_from(row["uid"].as_u64().unwrap()).unwrap();
                    let values = if event {
                        [
                            libc::SIGCHLD as u32,
                            0,
                            libc::CLD_EXITED as u32,
                            id as u32,
                            uid,
                            status as u32,
                        ]
                    } else {
                        [0; 6]
                    };
                    let mut info = [0xa5; 160];
                    for (offset, value) in [0, 4, 8, 16, 20, 24].into_iter().zip(values) {
                        info[16 + offset..20 + offset].copy_from_slice(&value.to_le_bytes());
                    }
                    assert_eq!(row["arena"], encode(&info));
                }
            }
        }
        assert_eq!(cpus.len(), 6);
        let adds = |before: (i64, i64), after: (i64, i64)| {
            assert!(after.0 >= before.0 && after.1 >= before.1 && after != before);
        };
        adds(cpus[0], cpus[1]);
        assert_eq!(cpus[1], cpus[2]);
        assert_eq!(cpus[1], cpus[3]);
        adds(cpus[3], cpus[4]);
        assert_eq!(cpus[4], cpus[5]);
    }
    assert_eq!(children.len(), 16);
    let summary = rows.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["cases"], 8);
    assert_eq!(summary["children"], 16);
    assert_eq!(summary["calls"], 96);
    assert_eq!(summary["passed"], true);
    assert!(summary["assertions"].as_u64().unwrap() > 400);
}

// INT_MIN is a raw pid_t argument error, after supported-option validation.
// Keep this separate from the eight consuming-fault/CPU lifecycle cases.
pub(super) fn run_wait4_int_min() {
    run_fixture(
        "wait4-int-min",
        "kvm_wait4_int_min.c",
        &[],
        assert_wait4_int_min_observations,
        |_, _| {},
    );
}

fn assert_wait4_int_min_observations(stdout: &[u8]) {
    let text = std::str::from_utf8(stdout).unwrap();
    assert!(text.ends_with('\n'));
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("complete raw wait4 observation"))
        .collect();
    let pids = [
        0x0000_0000_8000_0000_u64,
        0xffff_ffff_8000_0000,
        0x1234_5678_8000_0000,
        0x1234_5678_8000_0001,
    ];
    let options = [
        libc::WUNTRACED as u64,
        (libc::WNOHANG | libc::WUNTRACED) as u64,
        0_u64,
        libc::WNOHANG as u64,
        0x1234_5678_0000_0002,
        0xffff_ffff_0000_0003,
        0x100,
        0xfedc_ba98_0000_0102,
    ];
    assert_eq!(rows.len(), 65);
    let expected_arena = "a5".repeat(4096);
    let mut index = 0;
    for pid in pids {
        for option in options {
            for protected in 0..2_u64 {
                let row = &rows[index];
                assert_eq!(row["type"], "min-pid");
                assert_eq!(row["case"].as_u64(), Some(index as u64));
                assert_eq!(row["raw_pid"].as_u64(), Some(pid));
                assert_eq!(row["raw_options"].as_u64(), Some(option));
                assert_eq!(row["protected"].as_u64(), Some(protected));
                let expected = if option as u32 & !3 != 0 {
                    libc::EINVAL
                } else if pid as u32 == 0x8000_0000 {
                    libc::ESRCH
                } else {
                    libc::ECHILD
                };
                assert_eq!(row["rc"].as_i64(), Some(-1));
                assert_eq!(row["errno"].as_i64(), Some(i64::from(expected)));
                assert_eq!(row["status_arena"].as_str(), Some(expected_arena.as_str()));
                assert_eq!(row["usage_arena"].as_str(), Some(expected_arena.as_str()));
                index += 1;
            }
        }
    }
    assert_eq!(index, 64);
    assert_eq!(
        rows[64],
        serde_json::json!({
            "type": "summary", "cases": 64, "calls": 64, "children": 0, "passed": true
        })
    );
}

// Serial-KVM wait4 with `__WNOTHREAD`: the logical owner filter admits only
// the calling thread's own children. The guest checks every result and both
// whole output pages itself. This table independently fixes the complete
// call sequence, so a guest that skipped, added or reordered a call fails.
pub(super) fn run_wait4_nothread() {
    run_fixture(
        "wait4-nothread",
        "kvm_wait4_nothread.c",
        &[],
        assert_wait4_nothread_observations,
        |_, _| {},
    );
}

#[derive(Clone, Copy)]
enum NothreadTarget {
    Child(&'static str),
    /// wait4 pid -1.
    Any,
    /// waitid P_ALL.
    All,
}

#[derive(Clone, Copy)]
enum NothreadOutcome {
    Reaped(&'static str),
    Zero,
    Error(i32),
}

#[derive(Clone, Copy)]
enum NothreadCpu {
    Start,
    Equal,
    Added,
}

enum NothreadStep {
    Spawn {
        name: &'static str,
        status: i32,
        held: bool,
    },
    Wait4 {
        name: &'static str,
        target: NothreadTarget,
        options: i32,
        fault: i32,
        usage: bool,
        outcome: NothreadOutcome,
        status: i32,
    },
    Waitid {
        name: &'static str,
        target: NothreadTarget,
        options: i32,
        event: Option<(&'static str, i32)>,
        error: i32,
    },
    Cpu {
        name: &'static str,
        relation: NothreadCpu,
    },
    Case {
        name: &'static str,
        worker: bool,
    },
}

/// The complete expected sequence: (case, made by a worker thread, step).
fn wait4_nothread_script() -> Vec<(u32, bool, NothreadStep)> {
    use NothreadCpu::*;
    use NothreadOutcome::*;
    use NothreadStep::*;
    use NothreadTarget::*;
    const N: i32 = libc::WNOHANG;
    const U: i32 = libc::WUNTRACED;
    const T: i32 = libc::__WNOTHREAD;
    let wait4 = |name, target, options, usage, outcome, status| Wait4 {
        name,
        target,
        options,
        fault: 0,
        usage,
        outcome,
        status,
    };
    let echild = |name, target, options| wait4(name, target, options, true, Error(libc::ECHILD), 0);
    let peek = |name, child, status| Waitid {
        name,
        target: Child(child),
        options: libc::WEXITED | libc::WNOWAIT,
        event: Some((child, status)),
        error: 0,
    };
    let waitid_echild = |name, target| Waitid {
        name,
        target,
        options: libc::WEXITED | libc::WNOHANG,
        event: None,
        error: libc::ECHILD,
    };
    let cpu = |name, relation| Cpu { name, relation };
    let mut script = Vec::new();
    let deny = |script: &mut Vec<_>, case, worker, child| {
        for (name, target, options) in [
            ("deny-exact-nohang", Child(child), N | T),
            ("deny-any-nohang", Any, N | T),
            ("deny-exact-blocking", Child(child), T),
            ("deny-any-blocking", Any, T),
            ("deny-any-untraced", Any, U | T),
        ] {
            script.push((case, worker, echild(name, target, options)));
        }
    };
    let main = |script: &mut Vec<_>, case, steps: Vec<NothreadStep>| {
        script.extend(steps.into_iter().map(|step| (case, false, step)));
    };
    main(
        &mut script,
        0,
        vec![
            Spawn {
                name: "c1",
                status: 41,
                held: true,
            },
            wait4("own-exact-live", Child("c1"), N | T, true, Zero, 0),
            wait4("own-any-live", Any, N | T, true, Zero, 0),
            wait4(
                "own-exact-live-untraced",
                Child("c1"),
                N | U | T,
                true,
                Zero,
                0,
            ),
            wait4("own-any-live-untraced", Any, N | U | T, true, Zero, 0),
            cpu("baseline", Start),
            Case {
                name: "owned-live",
                worker: false,
            },
        ],
    );
    deny(&mut script, 1, true, "c1");
    main(
        &mut script,
        1,
        vec![
            cpu("after-live-denial", Equal),
            Case {
                name: "sibling-denial-live",
                worker: true,
            },
        ],
    );
    main(&mut script, 2, vec![peek("c1-peek", "c1", 41)]);
    deny(&mut script, 2, true, "c1");
    main(
        &mut script,
        2,
        vec![
            peek("c1-peek-again", "c1", 41),
            cpu("after-ready-denial", Equal),
            Case {
                name: "sibling-denial-ready",
                worker: true,
            },
        ],
    );
    main(
        &mut script,
        3,
        vec![
            wait4("own-exact-consume", Child("c1"), T, false, Reaped("c1"), 41),
            cpu("after-consume", Added),
            echild("own-exact-echild", Child("c1"), N | T),
            echild("own-any-echild", Any, N | T),
            waitid_echild("c1-echild", Child("c1")),
            cpu("after-echild", Equal),
            Case {
                name: "owned-ready-completion",
                worker: false,
            },
        ],
    );
    main(
        &mut script,
        4,
        vec![
            Spawn {
                name: "c2",
                status: 42,
                held: true,
            },
            wait4(
                "own-any-blocking-consume",
                Any,
                U | T,
                false,
                Reaped("c2"),
                42,
            ),
            cpu("after-consume", Added),
            echild("own-any-echild", Any, N | T),
            cpu("after-echild", Equal),
            Case {
                name: "owned-blocking-completion",
                worker: true,
            },
        ],
    );
    main(
        &mut script,
        5,
        vec![
            Spawn {
                name: "c3",
                status: 43,
                held: false,
            },
            peek("c3-peek", "c3", 43),
            wait4(
                "own-any-nohang-consume",
                Any,
                N | T,
                false,
                Reaped("c3"),
                43,
            ),
            cpu("after-consume", Added),
            echild("own-exact-echild", Child("c3"), N | T),
            cpu("after-echild", Equal),
            Case {
                name: "owned-nohang-completion",
                worker: false,
            },
        ],
    );
    // (fault, any-child selector, options, readiness peek first)
    let faults = [
        (1, false, T, false),
        (1, true, N | T, true),
        (2, false, U | T, false),
        (2, true, N | U | T, true),
    ];
    for (k, (fault, any, options, ready)) in faults.into_iter().enumerate() {
        let child = ["fault0", "fault1", "fault2", "fault3"][k];
        let status = 50 + k as i32;
        let mut steps = vec![Spawn {
            name: child,
            status,
            held: false,
        }];
        if ready {
            steps.push(peek("fault-peek", child, status));
        }
        steps.extend([
            cpu("before-fault", Equal),
            Wait4 {
                name: "own-consuming-fault",
                target: if any { Any } else { Child(child) },
                options,
                fault,
                usage: true,
                outcome: Error(libc::EFAULT),
                status,
            },
            cpu("after-fault", Added),
            echild("own-exact-echild", Child(child), N | T),
            waitid_echild("fault-echild", Child(child)),
            cpu("after-echild", Equal),
            Case {
                name: "owned-fault",
                worker: false,
            },
        ]);
        main(&mut script, 6 + k as u32, steps);
    }
    script.push((
        10,
        true,
        Spawn {
            name: "c5",
            status: 45,
            held: true,
        },
    ));
    deny(&mut script, 10, false, "c5");
    main(
        &mut script,
        10,
        vec![cpu("before-release", Equal), peek("c5-peek", "c5", 45)],
    );
    deny(&mut script, 10, false, "c5");
    main(
        &mut script,
        10,
        vec![peek("c5-peek-again", "c5", 45), cpu("after-denials", Equal)],
    );
    script.push((
        10,
        true,
        wait4(
            "creator-exact-consume",
            Child("c5"),
            T,
            false,
            Reaped("c5"),
            45,
        ),
    ));
    script.push((10, true, echild("creator-exact-echild", Child("c5"), N | T)));
    main(
        &mut script,
        10,
        vec![
            cpu("after-creator-consume", Added),
            echild("final-any-echild", Any, N),
            waitid_echild("final-all-echild", All),
            cpu("after-final-echild", Equal),
            Case {
                name: "reverse-sibling-denial",
                worker: true,
            },
        ],
    );
    script
}

fn assert_wait4_nothread_observations(stdout: &[u8]) {
    let rows: Vec<serde_json::Value> = std::str::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("complete __WNOTHREAD observation"))
        .collect();
    let script = wait4_nothread_script();
    assert_eq!(rows.len(), script.len() + 1);
    let summary = rows.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["cases"], 11);
    assert_eq!(summary["children"], 8);
    assert_eq!(summary["calls"], 55);
    assert_eq!(summary["assertions"], 440);
    assert_eq!(summary["passed"], true);
    let leader = summary["leader"].as_i64().unwrap();
    assert!(leader > 0);
    let workers: BTreeMap<u64, i64> = rows
        .iter()
        .filter(|row| row["type"] == "case")
        .map(|row| {
            (
                row["case"].as_u64().unwrap(),
                row["worker"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(workers.len(), 11);
    let encode = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let untouched = "a5".repeat(128);
    let mut children: BTreeMap<&str, i64> = BTreeMap::new();
    let mut worker_tids = BTreeSet::new();
    let mut cpu: Option<(i64, i64)> = None;
    let mut calls = 0;
    for (row, (case, worker, step)) in rows.iter().zip(&script) {
        assert_eq!(row["case"], *case, "{row}");
        let pid = |target: NothreadTarget| match target {
            NothreadTarget::Child(name) => children[name],
            NothreadTarget::Any => -1,
            NothreadTarget::All => 0,
        };
        if !matches!(step, NothreadStep::Case { .. }) {
            assert_eq!(
                row["actor"],
                if *worker { "worker" } else { "main" },
                "{row}"
            );
            if let Some(tid) = row.get("tid") {
                let expected = if *worker {
                    workers[&u64::from(*case)]
                } else {
                    leader
                };
                assert_eq!(tid.as_i64(), Some(expected), "{row}");
            }
        }
        match *step {
            NothreadStep::Spawn { name, status, held } => {
                assert_eq!(row["type"], "spawn");
                assert_eq!(row["name"], name);
                assert_eq!(row["status"], status);
                assert_eq!(row["held"], i32::from(held));
                let child = row["child"].as_i64().unwrap();
                assert!(child > 0 && child != leader);
                assert!(!children.values().any(|known| *known == child));
                assert!(children.insert(name, child).is_none());
            }
            NothreadStep::Wait4 {
                name,
                target,
                options,
                fault,
                usage,
                outcome,
                status,
            } => {
                calls += 1;
                assert_eq!(row["type"], "wait4");
                assert_eq!(row["name"], name);
                assert_eq!(row["selector"], pid(target), "{row}");
                assert_eq!(row["options"], options, "{row}");
                assert_eq!(row["fault"], fault);
                assert_eq!(row["usage"], i32::from(usage));
                let (rc, errno) = match outcome {
                    NothreadOutcome::Reaped(child) => (children[child], 0),
                    NothreadOutcome::Zero => (0, 0),
                    NothreadOutcome::Error(errno) => (-1, errno),
                };
                assert_eq!(row["rc"], rc, "{row}");
                assert_eq!(row["errno"], errno, "{row}");
                let mut window = [0xa5_u8; 128];
                if matches!(outcome, NothreadOutcome::Reaped(_)) || fault == 2 {
                    window[64..68].copy_from_slice(&(status << 8).to_le_bytes());
                }
                assert_eq!(row["status_window"], encode(&window), "{row}");
                assert_eq!(row["usage_window"], untouched, "{row}");
            }
            NothreadStep::Waitid {
                name,
                target,
                options,
                event,
                error,
            } => {
                calls += 1;
                assert_eq!(row["type"], "waitid");
                assert_eq!(row["name"], name);
                let which = if matches!(target, NothreadTarget::All) {
                    libc::P_ALL
                } else {
                    libc::P_PID
                };
                assert_eq!(row["which"], which);
                assert_eq!(row["id"], pid(target));
                assert_eq!(row["options"], options);
                assert_eq!(row["rc"], if error == 0 { 0 } else { -1 }, "{row}");
                assert_eq!(row["errno"], error, "{row}");
                let (signo, code, child, status) = match event {
                    Some((child, status)) => {
                        (libc::SIGCHLD, libc::CLD_EXITED, children[child], status)
                    }
                    None => (0, 0, 0, 0),
                };
                assert_eq!(row["si_signo"], signo, "{row}");
                assert_eq!(row["si_code"], code, "{row}");
                assert_eq!(row["si_pid"], child, "{row}");
                assert_eq!(row["si_status"], status, "{row}");
            }
            NothreadStep::Cpu { name, relation } => {
                assert_eq!(row["type"], "cpu");
                assert_eq!(row["name"], name);
                let now = (
                    row["user_us"].as_i64().unwrap(),
                    row["system_us"].as_i64().unwrap(),
                );
                assert!(now.0 >= 0 && now.1 >= 0);
                match (relation, cpu) {
                    (NothreadCpu::Start, None) => {}
                    (NothreadCpu::Equal, Some(before)) => assert_eq!(now, before, "{row}"),
                    (NothreadCpu::Added, Some(before)) => assert!(
                        now.0 >= before.0 && now.1 >= before.1 && now != before,
                        "consuming wait adds child CPU once: {row}"
                    ),
                    _ => panic!("CPU relation out of order: {row}"),
                }
                cpu = Some(now);
            }
            NothreadStep::Case { name, worker } => {
                assert_eq!(row["type"], "case");
                assert_eq!(row["name"], name);
                assert_eq!(row["passed"], true);
                let tid = row["worker"].as_i64().unwrap();
                if worker {
                    assert!(tid > 0 && tid != leader, "{row}");
                    assert!(worker_tids.insert(tid), "one fresh worker per case: {row}");
                } else {
                    assert_eq!(tid, 0, "{row}");
                }
            }
        }
    }
    assert_eq!(calls, 55);
    assert_eq!(children.len(), 8);
    assert_eq!(worker_tids.len(), 4);
}

// Serial-KVM foreign waiters contend for children that the leader holds alive
// on a pipe. The guest checks the Linux results. This harness proves, from
// both retained INFO logs, the scheduling facts the guest cannot see:
//
// * Every contender entered the child-wait pool after its wait began and
//   before the leader's release write began. The park record is the witness:
//   the scheduler emits it exactly when it inserts the thread into
//   `blocked.child_waiters`.
// * Each blocking wait made one WaitChild request. The scheduler parked it,
//   parked it again on the same request if another waiter took the child
//   first, and granted it once, after its last park. That grant is the last
//   scheduler decision of any thread before the wait's result, so no other
//   thread ran between the winner's selection and its consumption.
// * The foreign `__WNOTHREAD` waits for the leader's live child are refused
//   without a park, before that child could exit, and the foreign thread's
//   blocking wait for its own child stays parked through the leader's
//   child's exit.
//
// A guest sleep or yield proves none of this. If any contender had not parked
// before its child could exit, the test fails.
//
// Windows, and how each is covered:
//
// * Child exit before a contender parks: excluded by the guest-causal
//   handoff. A child exits only after the leader's release write, and the
//   check below requires every park to precede that write's entry.
// * Exit publication against selection: exercised. The scheduler records
//   the logical exit and wakes every matching parked waiter in one step
//   (`logically_kill_thread` calling `wake_child_waiters`), admitting the
//   woken waiters in dettid order. The loser of each race then gets ECHILD
//   (case 1) or parks again for the next child (case 3).
// * Grant against consumption: excluded. The ready check, the private
//   proof, the CPU publication check and the consumption run in the turn
//   the WaitChild grant opens; the check below requires that no thread's
//   scheduler decision falls between the final grant and the result.
// * Child CPU publication: crossed by every consuming wait; the guest
//   requires exactly one child's CPU time added per reap.
// * Signal interruption: excluded, because every thread blocks SIGCHLD. The
//   interrupted-wait paths are the waitid error fixture's EINTR and
//   SA_RESTART cases.
// * Scheduler quiescence: excluded. The leader stays runnable until each
//   release, so the all-blocked check never runs during a contention.
// * Physical-child waits: KVM never requests one; the check below fails on
//   any physical-child park.
pub(super) fn run_parked_foreign_waiters(family: &'static str) {
    assert!(matches!(family, "waitid" | "wait4"));
    run_fixture(
        &format!("parked-foreign-waiters-{family}"),
        "kvm_wait_contenders.c",
        &[family],
        |stdout| {
            contenders_from_stdout(stdout, family);
        },
        |log, stdout| assert_contender_parks(log, &contenders_from_stdout(stdout, family), family),
    );
}

/// The thread and child identities and outcomes that the guest printed.
struct Contenders {
    leader: u64,
    tid: BTreeMap<&'static str, u64>,
    /// Child name to (pid, exit status, release write descriptor).
    child: BTreeMap<&'static str, (u64, i64, u64)>,
    case1_winner: &'static str,
    case1_loser: &'static str,
    observed: bool,
    case3_winner: &'static str,
    case3_next: &'static str,
}

fn contenders_from_stdout(stdout: &[u8], family: &str) -> Contenders {
    const UNWRITTEN: i64 = 0x5a5a5a5a;
    let text = std::str::from_utf8(stdout).expect("complete contender fixture UTF-8");
    assert!(text.ends_with('\n'));
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON row"))
        .collect();
    assert_eq!(
        rows.len(),
        54,
        "7 threads, 6 spawns, 17 waits, 16 CPU rows, 3 outcomes, 4 cases, 1 summary"
    );
    let of_type = |kind: &str| -> Vec<&serde_json::Value> {
        rows.iter().filter(|row| row["type"] == kind).collect()
    };
    let summary = rows.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["family"], family);
    for (key, value) in [
        ("cases", 4),
        ("children", 6),
        ("threads", 7),
        ("calls", 17),
        ("assertions", 277),
    ] {
        assert_eq!(summary[key], value, "summary {key}");
    }
    assert_eq!(summary["passed"], true);
    let leader = summary["leader"].as_u64().unwrap();
    assert!(leader > 0);

    let threads = of_type("thread");
    assert_eq!(threads.len(), 7);
    let mut tid = BTreeMap::new();
    for (name, case, kind) in [
        ("N", 1, 4),
        ("A", 1, 1),
        ("B", 1, 1),
        ("O", 2, 3),
        ("Q", 2, 1),
        ("X", 3, 2),
        ("Y", 3, 2),
    ] {
        let found: Vec<_> = threads.iter().filter(|row| row["name"] == name).collect();
        assert_eq!(found.len(), 1, "one thread row for {name}");
        assert_eq!(found[0]["case"], case);
        assert_eq!(found[0]["kind"], kind);
        let value = found[0]["tid"].as_u64().unwrap();
        assert!(value > 0 && value != leader, "{name} is a sibling thread");
        tid.insert(name, value);
    }
    assert_eq!(
        tid.values().collect::<BTreeSet<_>>().len(),
        7,
        "distinct threads"
    );

    let spawns = of_type("spawn");
    assert_eq!(spawns.len(), 6);
    let mut child = BTreeMap::new();
    for (name, case, status, actor) in [
        ("K", 0, 60, "main"),
        ("C", 1, 61, "main"),
        ("D", 1, 62, "N"),
        ("E", 2, 63, "main"),
        ("F", 3, 64, "main"),
        ("G", 3, 65, "main"),
    ] {
        let found: Vec<_> = spawns.iter().filter(|row| row["name"] == name).collect();
        assert_eq!(found.len(), 1, "one spawn row for {name}");
        let row = found[0];
        assert_eq!(row["case"], case);
        assert_eq!(row["status"], status);
        assert_eq!(row["actor"], actor);
        let creator = if actor == "main" { leader } else { tid[actor] };
        assert_eq!(row["tid"], creator, "{name} was created by {actor}");
        let pid = row["child"].as_u64().unwrap();
        let fd = row["release_fd"].as_u64().unwrap();
        assert!(pid > 0 && fd > 2);
        child.insert(name, (pid, status, fd));
    }
    assert_eq!(
        child
            .values()
            .map(|value| value.0)
            .collect::<BTreeSet<_>>()
            .len(),
        6
    );
    assert_eq!(
        child
            .values()
            .map(|value| value.2)
            .collect::<BTreeSet<_>>()
            .len(),
        6
    );

    let waits = of_type("wait");
    assert_eq!(waits.len(), 17);
    let wait = |case: u64, actor: &str, name: &str| -> &serde_json::Value {
        let found: Vec<_> = waits
            .iter()
            .filter(|row| row["case"] == case && row["actor"] == actor && row["name"] == name)
            .collect();
        assert_eq!(found.len(), 1, "one wait row: case {case} {actor} {name}");
        let row = found[0];
        let thread = if actor == "main" { leader } else { tid[actor] };
        assert_eq!(row["tid"], thread);
        row
    };
    // The exact call: target None is pid -1 or P_ALL.
    let shape = |row: &serde_json::Value, call: &str, target: Option<&str>, options: i64| {
        assert_eq!(row["call"], call, "{row}");
        let pid = target.map(|name| child[name].0);
        if call == "waitid" {
            assert_eq!(
                row["which"],
                if pid.is_some() {
                    libc::P_PID
                } else {
                    libc::P_ALL
                },
                "{row}"
            );
            assert_eq!(row["id"], pid.unwrap_or(0), "{row}");
        } else {
            assert_eq!(row["pid"], pid.map_or(-1, |pid| pid as i64), "{row}");
        }
        assert_eq!(row["options"], options, "{row}");
    };
    let family_shape =
        |row: &serde_json::Value, target: Option<&str>, nohang: bool, nothread: bool| {
            let mut options = 0;
            if nohang {
                options |= libc::WNOHANG;
            }
            if nothread {
                options |= libc::__WNOTHREAD;
            }
            if family == "waitid" {
                options |= libc::WEXITED;
            }
            shape(row, family, target, i64::from(options));
        };
    let reaped = |row: &serde_json::Value, name: &str| -> bool {
        let (pid, status, _) = child[name];
        if row["call"] == "waitid" {
            row["rc"] == 0
                && row["errno"] == 0
                && row["si_signo"] == libc::SIGCHLD
                && row["si_code"] == libc::CLD_EXITED
                && row["si_pid"] == pid
                && row["si_status"] == status
        } else {
            row["rc"] == pid && row["errno"] == 0 && row["raw_status"] == status << 8
        }
    };
    let echild = |row: &serde_json::Value| -> bool {
        if row["call"] == "waitid" {
            row["rc"] == -1
                && row["errno"] == libc::ECHILD
                && row["si_signo"] == 0
                && row["si_code"] == 0
                && row["si_pid"] == 0
                && row["si_status"] == 0
        } else {
            row["rc"] == -1 && row["errno"] == libc::ECHILD && row["raw_status"] == UNWRITTEN
        }
    };
    let live = |row: &serde_json::Value| -> bool {
        if row["call"] == "waitid" {
            row["rc"] == 0
                && row["errno"] == 0
                && row["si_signo"] == 0
                && row["si_code"] == 0
                && row["si_pid"] == 0
                && row["si_status"] == 0
        } else {
            row["rc"] == 0 && row["errno"] == 0 && row["raw_status"] == UNWRITTEN
        }
    };

    let row = wait(0, "main", "control-consume");
    family_shape(row, Some("K"), false, false);
    assert!(reaped(row, "K"), "{row}");
    let row = wait(0, "main", "control-echild");
    family_shape(row, Some("K"), true, false);
    assert!(echild(row), "{row}");
    for name in ["deny-nohang", "deny-blocking"] {
        let row = wait(1, "N", name);
        family_shape(row, Some("C"), name == "deny-nohang", true);
        assert!(
            echild(row),
            "foreign __WNOTHREAD never selects the leader's child: {row}"
        );
    }
    let row = wait(1, "N", "own-live");
    family_shape(row, None, true, true);
    assert!(live(row), "{row}");
    let row = wait(1, "N", "own-consume");
    family_shape(row, None, false, true);
    assert!(
        reaped(row, "D"),
        "the foreign waiter reaps only its own child: {row}"
    );
    let row = wait(1, "N", "own-echild");
    family_shape(row, None, true, true);
    assert!(echild(row), "{row}");
    for (case, name) in [(1, "C"), (1, "D"), (2, "E")] {
        let label = if name == "D" {
            "foreign-echild"
        } else {
            "later-echild"
        };
        let row = wait(case, "main", label);
        family_shape(row, Some(name), true, false);
        assert!(echild(row), "{name} was consumed once: {row}");
    }
    let row = wait(3, "main", "later-echild");
    family_shape(row, None, true, false);
    assert!(echild(row), "no child remains: {row}");

    let a = wait(1, "A", "contend");
    let b = wait(1, "B", "contend");
    for row in [a, b] {
        family_shape(row, Some("C"), false, false);
    }
    let (case1_winner, case1_loser) = match (reaped(a, "C"), reaped(b, "C")) {
        (true, false) => ("A", "B"),
        (false, true) => ("B", "A"),
        other => panic!("exactly one consumer reaps C: {other:?}"),
    };
    assert!(
        echild(if case1_winner == "A" { b } else { a }),
        "the other consumer gets ECHILD"
    );

    let o = wait(2, "O", "observe");
    shape(
        o,
        "waitid",
        Some("E"),
        i64::from(libc::WEXITED | libc::WNOWAIT),
    );
    let observed = reaped(o, "E");
    assert!(
        observed || echild(o),
        "the observer sees E or finds it consumed: {o}"
    );
    let q = wait(2, "Q", "contend");
    family_shape(q, Some("E"), false, false);
    assert!(reaped(q, "E"), "the only consumer reaps E: {q}");

    let x = wait(3, "X", "contend");
    let y = wait(3, "Y", "contend");
    for row in [x, y] {
        family_shape(row, None, false, false);
    }
    let (case3_winner, case3_next) = match (reaped(x, "F"), reaped(y, "F")) {
        (true, false) => ("X", "Y"),
        (false, true) => ("Y", "X"),
        other => panic!("exactly one any-child consumer reaps F: {other:?}"),
    };
    assert!(
        reaped(if case3_winner == "X" { y } else { x }, "G"),
        "the loser reaps the next child G"
    );

    let outcomes = of_type("outcome");
    assert_eq!(outcomes.len(), 3);
    let outcome = |case: u64| -> &serde_json::Value {
        let found: Vec<_> = outcomes.iter().filter(|row| row["case"] == case).collect();
        assert_eq!(found.len(), 1);
        found[0]
    };
    let row = outcome(1);
    assert_eq!(row["winner"], case1_winner);
    assert_eq!(row["winner_tid"], tid[case1_winner]);
    assert_eq!(row["loser"], case1_loser);
    assert_eq!(row["loser_tid"], tid[case1_loser]);
    let row = outcome(2);
    assert_eq!(
        row["observer"],
        if observed { "observed" } else { "echild" }
    );
    assert_eq!(row["observer_tid"], tid["O"]);
    assert_eq!(row["consumer_tid"], tid["Q"]);
    let row = outcome(3);
    assert_eq!(row["winner"], case3_winner);
    assert_eq!(row["winner_tid"], tid[case3_winner]);
    assert_eq!(row["next"], case3_next);
    assert_eq!(row["next_tid"], tid[case3_next]);

    // Children CPU: '0' none yet, '=' unchanged, '+' exactly one more child's.
    // The children run identical code, so every reap adds about the control
    // reap's amount; two children's worth, or none, falls outside the band.
    let cpu = of_type("cpu");
    let expected = [
        (0, "before-release", '0'),
        (0, "after-reap", '+'),
        (0, "after-echild", '='),
        (1, "before-release", '='),
        (1, "after-contest", '+'),
        (1, "after-echild", '='),
        (1, "after-foreign-reap", '+'),
        (1, "after-foreign-echild", '='),
        (2, "before-release", '='),
        (2, "after-pair", '+'),
        (2, "after-echild", '='),
        (3, "before-release", '='),
        (3, "after-first", '+'),
        (3, "before-second-release", '='),
        (3, "after-second", '+'),
        (3, "after-echild", '='),
    ];
    assert_eq!(cpu.len(), expected.len());
    let mut previous = (0, 0);
    let mut unit = None;
    for (row, (case, name, relation)) in cpu.iter().zip(expected) {
        assert_eq!(row["case"], case);
        assert_eq!(row["name"], name);
        assert_eq!(row["actor"], "main");
        let now = (
            row["user_us"].as_i64().unwrap(),
            row["system_us"].as_i64().unwrap(),
        );
        match relation {
            '0' => assert_eq!(now, (0, 0), "no child CPU before the first reap"),
            '=' => assert_eq!(now, previous, "children CPU unchanged: {row}"),
            _ => {
                assert!(now.0 >= previous.0 && now.1 >= previous.1, "{row}");
                let added = (now.0 - previous.0) + (now.1 - previous.1);
                let unit = *unit.get_or_insert(added);
                assert!(unit > 0, "the control reap adds child CPU");
                assert!(
                    2 * added > unit && 2 * added < 3 * unit,
                    "one child's CPU per reap ({added} us against {unit} us): {row}"
                );
            }
        }
        previous = now;
    }

    let cases = of_type("case");
    let names = [
        "control",
        "two-consumers-and-foreign-nothread",
        "observer-and-consumer",
        "any-consumers-next-child",
    ];
    assert_eq!(cases.len(), names.len());
    for (number, (row, name)) in cases.iter().zip(names).enumerate() {
        assert_eq!(row["case"], number);
        assert_eq!(row["name"], name);
        assert_eq!(row["passed"], true);
    }
    Contenders {
        leader,
        tid,
        child,
        case1_winner,
        case1_loser,
        observed,
        case3_winner,
        case3_next,
    }
}

enum TraceEvent {
    Entry(u64, String),
    Finish(u64, String, String),
    /// A child-wait pool entry: dettid and its spec.
    Park(u64, String),
    /// The WaitChild commit that ends a child wait's scheduler request.
    Grant(u64, String),
    /// Any other scheduler decision, of any thread.
    Decision,
}

fn assert_contender_parks(log: &[u8], run: &Contenders, family: &str) {
    let records = info_records(std::str::from_utf8(log).expect("complete UTF-8 INFO log"));
    let entry = regex::Regex::new(
        r"^DETLOG \[syscall\]\[detcore, dtid (\d+)\] inbound syscall: (.+) = \?$",
    )
    .unwrap();
    let finish = regex::Regex::new(
        r"^DETLOG \[syscall\]\[detcore, dtid (\d+)\] finish syscall #(\d+): (.+) = (.+)$",
    )
    .unwrap();
    let park = regex::Regex::new(
        r"^\[scheduler\] NONCOMMIT turn \d+, parking dettid (\d+) for (child (ChildWaitSpec \{ [^\n]+ \})|physical child \d+)$",
    )
    .unwrap();
    let grant = regex::Regex::new(&format!(
        r"(?s)^\[sched-step5\] >>>>>>>\n\n COMMIT turn \d+, dettid (\d+) using resources \{{WaitChild \{{ parent: DetPid\({}\), spec: (ChildWaitSpec \{{ [^\n]+ \}}) \}}: R\}}, on previously committed \S+$",
        run.leader
    ))
    .unwrap();
    let decision =
        regex::Regex::new(r"(?s)^\[(scheduler|sched-step5)\] .*COMMIT turn \d+").unwrap();
    let mut events = Vec::new();
    for (index, record) in records.iter().enumerate() {
        if let Some(capture) = park.captures(record) {
            let spec = capture.get(3).map(|spec| spec.as_str().to_owned());
            // KVM reports no physical exits, so no wait parks for one.
            let spec = spec.unwrap_or_else(|| panic!("unexpected physical-child park: {record}"));
            events.push((index, TraceEvent::Park(capture[1].parse().unwrap(), spec)));
            continue;
        }
        let Some((message, metadata)) = record.rsplit_once(" DETLOG_RECORD=") else {
            if decision.is_match(record) {
                events.push((index, TraceEvent::Decision));
            }
            continue;
        };
        let value: serde_json::Value = serde_json::from_str(metadata).unwrap();
        assert_eq!(value["schema"], 1);
        if let Some(capture) = entry.captures(message) {
            assert_eq!(value["event"]["kind"], "syscall");
            events.push((
                index,
                TraceEvent::Entry(capture[1].parse().unwrap(), capture[2].to_owned()),
            ));
        } else if let Some(capture) = finish.captures(message) {
            assert_eq!(value["event"]["kind"], "syscall_result");
            assert_eq!(
                value["event"]["finished_syscall_number"].as_u64(),
                Some(capture[2].parse::<u64>().unwrap())
            );
            events.push((
                index,
                TraceEvent::Finish(
                    capture[1].parse().unwrap(),
                    capture[3].to_owned(),
                    capture[4].to_owned(),
                ),
            ));
        } else if value["event"]["kind"] == "scheduler_commit" {
            match grant.captures(message) {
                Some(capture) => events.push((
                    index,
                    TraceEvent::Grant(capture[1].parse().unwrap(), capture[2].to_owned()),
                )),
                None => events.push((index, TraceEvent::Decision)),
            }
        } else if decision.is_match(message) {
            events.push((index, TraceEvent::Decision));
        }
    }

    // Every call of `tid` matching `pattern`: (entry index, result index, result).
    let calls = |tid: u64, pattern: &str| -> Vec<(usize, usize, String)> {
        let expression = regex::Regex::new(pattern).unwrap();
        let mut found = Vec::new();
        for (position, (begin, event)) in events.iter().enumerate() {
            let TraceEvent::Entry(actual, call) = event else {
                continue;
            };
            if *actual != tid || !expression.is_match(call) {
                continue;
            }
            let (end, result) = events[position + 1..]
                .iter()
                .find_map(|(index, event)| match event {
                    TraceEvent::Entry(other, next) if *other == tid => {
                        panic!("{call}: dettid {tid} entered {next} before its result")
                    }
                    TraceEvent::Finish(other, finished, result) if *other == tid => {
                        assert_eq!(finished, call);
                        Some((*index, result.clone()))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{call}: dettid {tid} has no result"));
            found.push((*begin, end, result));
        }
        found
    };
    let one = |tid: u64, pattern: String, result: &str| -> (usize, usize) {
        let found = calls(tid, &pattern);
        assert_eq!(found.len(), 1, "one call by dettid {tid}: {pattern}");
        assert_eq!(found[0].2, result, "{pattern}");
        (found[0].0, found[0].1)
    };
    let release = |name: &str| -> usize {
        let fd = run.child[name].2;
        one(
            run.leader,
            format!(r"^write\({fd}, 0x[0-9a-f]+, 1\)$"),
            "Ok(1)",
        )
        .0
    };
    // The family's call text: target None is pid -1 or P_ALL.
    let pattern = |target: Option<&str>, nohang: bool, nothread: bool| -> String {
        let pid = target.map(|name| run.child[name].0);
        if family == "waitid" {
            let mut options = libc::WEXITED;
            if nohang {
                options |= libc::WNOHANG;
            }
            if nothread {
                options |= libc::__WNOTHREAD;
            }
            match pid {
                Some(pid) => format!(r"^waitid\(1, {pid}, 0x[0-9a-f]+, {options}, NULL\)$"),
                None => format!(r"^waitid\(0, 0, 0x[0-9a-f]+, {options}, NULL\)$"),
            }
        } else {
            let flags = match (nohang, nothread) {
                (false, false) => "0x0",
                (true, false) => "WNOHANG",
                (false, true) => "__WNOTHREAD",
                (true, true) => r"WNOHANG \| __WNOTHREAD",
            };
            let pid = pid.map_or(-1, |pid| pid as i64);
            format!(r"^wait4\({pid}, 0x[0-9a-f]+, WaitPidFlag\({flags}\), NULL\)$")
        }
    };
    let success = |name: &str| -> String {
        if family == "waitid" {
            "Ok(0)".to_owned()
        } else {
            format!("Ok({})", run.child[name].0)
        }
    };
    const ECHILD: &str = "Err(Errno(ECHILD))";
    let exact = |name: &str| {
        format!(
            "ChildWaitSpec {{ selector: Exact(DetPid({})), owner: None, exit_class: Sigchld }}",
            run.child[name].0
        )
    };
    let any = "ChildWaitSpec { selector: Any, owner: None, exit_class: Sigchld }".to_owned();

    // One blocking wait: park i lies inside bounds[i]. Its one request is
    // granted once, after the last park; a woken waiter whose child was taken
    // is parked again by the scheduler on that same request. The grant is
    // the last scheduler decision of any thread before the result, so the
    // ready check, the private proof and the consumption share one turn.
    let parked = |tid: u64, window: (usize, usize), spec: &str, bounds: &[(usize, usize)]| {
        let mut parks = Vec::new();
        let mut grants = Vec::new();
        for (index, event) in &events {
            if *index <= window.0 || *index >= window.1 {
                continue;
            }
            match event {
                TraceEvent::Park(actual, actual_spec) if *actual == tid => {
                    assert_eq!(actual_spec, spec, "dettid {tid} parked with its own spec");
                    parks.push(*index);
                }
                TraceEvent::Grant(actual, actual_spec) if *actual == tid => {
                    assert_eq!(actual_spec, spec, "dettid {tid} granted its own spec");
                    grants.push(*index);
                }
                _ => {}
            }
        }
        assert_eq!(
            parks.len(),
            bounds.len(),
            "dettid {tid}: pool entries in {window:?}"
        );
        assert_eq!(grants.len(), 1, "dettid {tid}: one granted request");
        let last = grants[0];
        for (number, (park, (low, high))) in parks.iter().zip(bounds).enumerate() {
            assert!(
                low < park && park < high,
                "dettid {tid} park {number} at {park} outside ({low}, {high})"
            );
            assert!(*park < last, "dettid {tid}: granted after park {number}");
        }
        assert!(
            !events.iter().any(|(index, event)| {
                last < *index
                    && *index < window.1
                    && matches!(
                        event,
                        TraceEvent::Park(..) | TraceEvent::Grant(..) | TraceEvent::Decision
                    )
            }),
            "dettid {tid}: no scheduler decision between its final grant and its result"
        );
    };

    let tid = |name: &str| run.tid[name];
    let (rel_c, rel_d, rel_e, rel_f, rel_g) = (
        release("C"),
        release("D"),
        release("E"),
        release("F"),
        release("G"),
    );
    assert!(rel_c < rel_d && rel_d < rel_e && rel_e < rel_f && rel_f < rel_g);
    let start = 0;

    // Case 1: the foreign __WNOTHREAD waits for the leader's live child C are
    // refused before C could exit. The blocking one makes one scheduler
    // request, granted at once because nothing it may select exists; no park.
    let n = tid("N");
    let refused = one(n, pattern(Some("C"), true, true), ECHILD);
    let refused_blocking = one(n, pattern(Some("C"), false, true), ECHILD);
    assert!(refused_blocking.1 < rel_c, "refused before C could exit");
    let owner_scoped = format!(
        "ChildWaitSpec {{ selector: Exact(DetPid({})), owner: Some(DetPid({n})), exit_class: Sigchld }}",
        run.child["C"].0
    );
    let parks_and_grants = |window: (usize, usize)| -> (usize, Vec<String>) {
        let mut parks = 0;
        let mut grants = Vec::new();
        for (index, event) in &events {
            if window.0 < *index && *index < window.1 {
                match event {
                    TraceEvent::Park(actual, _) if *actual == n => parks += 1,
                    TraceEvent::Grant(actual, spec) if *actual == n => grants.push(spec.clone()),
                    _ => {}
                }
            }
        }
        (parks, grants)
    };
    assert_eq!(
        parks_and_grants(refused),
        (0, vec![]),
        "nonblocking refusal needs no turn"
    );
    assert_eq!(parks_and_grants(refused_blocking), (0, vec![owner_scoped]));
    let probes = calls(n, &pattern(None, true, true));
    assert_eq!(probes.len(), 2, "own-live and own-echild");
    assert_eq!(probes[0].2, "Ok(0)");
    assert_eq!(probes[1].2, ECHILD);
    // N's own blocking wait enters the pool before C's release, stays parked
    // through C's exit and consumption, and completes only after D's release.
    let own = one(n, pattern(None, false, true), &success("D"));
    let own_spec =
        format!("ChildWaitSpec {{ selector: Any, owner: Some(DetPid({n})), exit_class: Sigchld }}");
    parked(n, own, &own_spec, &[(start, rel_c)]);
    assert!(probes[0].1 < own.0 && own.1 < probes[1].0 && rel_d < own.1);

    // Two exact consumers for C: both parked before the release; the winner
    // is the guest-reported one and the loser's single park ends in ECHILD.
    let mut finished = Vec::new();
    for (name, result) in [
        (run.case1_winner, success("C")),
        (run.case1_loser, ECHILD.to_owned()),
    ] {
        let window = one(tid(name), pattern(Some("C"), false, false), &result);
        parked(tid(name), window, &exact("C"), &[(start, rel_c)]);
        assert!(rel_c < window.1 && window.1 < rel_d);
        finished.push(window.1);
    }
    assert!(
        finished[0] < finished[1],
        "the winner's result precedes the loser's"
    );

    // Case 2: an observer and a consumer, both parked before E's release.
    let observer_result = if run.observed { "Ok(0)" } else { ECHILD };
    let observer = one(
        tid("O"),
        format!(
            r"^waitid\(1, {}, 0x[0-9a-f]+, {}, NULL\)$",
            run.child["E"].0,
            libc::WEXITED | libc::WNOWAIT
        ),
        observer_result,
    );
    parked(tid("O"), observer, &exact("E"), &[(rel_d, rel_e)]);
    let consumer = one(tid("Q"), pattern(Some("E"), false, false), &success("E"));
    parked(tid("Q"), consumer, &exact("E"), &[(rel_d, rel_e)]);
    assert!(rel_e < observer.1 && rel_e < consumer.1 && observer.1 < rel_f && consumer.1 < rel_f);

    // Case 3: both any-child consumers parked before F's release; the loser
    // parked again after F's release and reaped G after G's release.
    let winner = one(
        tid(run.case3_winner),
        pattern(None, false, false),
        &success("F"),
    );
    parked(tid(run.case3_winner), winner, &any, &[(rel_e, rel_f)]);
    assert!(rel_f < winner.1 && winner.1 < rel_g);
    let next = one(
        tid(run.case3_next),
        pattern(None, false, false),
        &success("G"),
    );
    parked(
        tid(run.case3_next),
        next,
        &any,
        &[(rel_e, rel_f), (rel_f, rel_g)],
    );
    assert!(rel_g < next.1);

    // No contender entered the pool anywhere else.
    for (name, expected) in [
        ("N", 1),
        ("A", 1),
        ("B", 1),
        ("O", 1),
        ("Q", 1),
        (run.case3_winner, 1),
        (run.case3_next, 2),
    ] {
        let total = events
            .iter()
            .filter(
                |(_, event)| matches!(event, TraceEvent::Park(actual, _) if *actual == tid(name)),
            )
            .count();
        assert_eq!(
            total, expected,
            "{name}: child-wait pool entries in the whole log"
        );
    }
}
