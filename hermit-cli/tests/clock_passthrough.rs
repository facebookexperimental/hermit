/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Clock reads without virtual time. Record/replay never virtualizes time (see
//! `metadata::record_or_replay_config`), so a recording must capture the host
//! clock values its guest observed and replay must return exactly those.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;

/// Clock reads plus the lseek witness after each printed value.
const CLOCK_DETLOG_LINES: usize = 10 + 15;

fn build_guest() -> PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("clock-passthrough");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let guest = build_root.join("clock-passthrough");
    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/clock_passthrough.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile clock passthrough guest");
    assert!(
        compile.status.success(),
        "guest compilation failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    guest
}

fn hermit(args: &[&str], guest: Option<&Path>) -> Output {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "60s"])
        .arg(hermit_test::hermit_binary())
        .args(args);
    if let Some(guest) = guest {
        command.arg("--").arg(guest);
    }
    hermit_test::configure_guest_execution(&mut command);
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to start hermit {args:?}: {error}"))
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("clock-passthrough-ok"),
        "{label} failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn reported(output: &Output, name: &str) -> u64 {
    let prefix = format!("{name}=");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&prefix)?.parse().ok())
        .unwrap_or_else(|| panic!("guest did not report {name}"))
}

fn host_seconds() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The guest read the host clock rather than Hermit's virtual epoch.
fn assert_host_realtime(label: &str, output: &Output) {
    let observed = reported(output, "realtime.sec");
    let host = host_seconds();
    assert!(
        observed.abs_diff(host) < 600,
        "{label} reported realtime {observed}, host time is {host}"
    );
}

/// The finished clock reads and witness lseeks from a DETLOG trace. Each entry
/// shows the value the syscall wrote into guest memory or returned.
fn clock_detlog(output: &Output) -> Vec<String> {
    const CALLS: [&str; 5] = [
        ": clock_gettime(",
        ": clock_getres(",
        ": gettimeofday(",
        ": time(",
        ": lseek(",
    ];
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| {
            let entry = &line[line.find("DETLOG [syscall]")?..];
            let entry = &entry[entry.find("finish syscall #")?..];
            let entry = entry.split(" DETLOG_RECORD=").next().unwrap();
            CALLS
                .iter()
                .any(|call| entry.contains(call))
                .then(|| entry.to_owned())
        })
        .collect()
}

#[test]
fn record_captures_host_clock_reads_and_replay_returns_them() {
    let guest = build_guest();
    let data_dir = tempfile::tempdir().expect("failed to create recording directory");
    let data_dir_arg = format!("--data-dir={}", data_dir.path().display());

    let recorded = hermit(
        &[
            "--log=info",
            "record",
            "start",
            "--record-timeout=45",
            &data_dir_arg,
        ],
        Some(&guest),
    );
    assert_success("recording", &recorded);
    assert_host_realtime("recording", &recorded);
    let recorded_detlog = clock_detlog(&recorded);
    assert_eq!(
        recorded_detlog.len(),
        CLOCK_DETLOG_LINES,
        "{recorded_detlog:#?}"
    );

    // Let the host clock move past the recorded `time()` value, so a replay that
    // read the host clock would observe a different value.
    let recorded_time = reported(&recorded, "time");
    while host_seconds() <= recorded_time {
        thread::sleep(Duration::from_millis(100));
    }

    for attempt in 1..=2 {
        let replayed = hermit(
            &["--log=info", "replay", "--autopilot", &data_dir_arg],
            None,
        );
        let label = format!("replay {attempt}");
        assert_success(&label, &replayed);
        assert_eq!(replayed.stdout, recorded.stdout, "{label}");
        assert_eq!(clock_detlog(&replayed), recorded_detlog, "{label}");
    }
}

#[test]
fn run_without_virtual_time_reads_the_host_clock() {
    let guest = build_guest();
    for strict in [false, true] {
        let mut args = vec!["run", "--no-virtualize-time", "--no-virtualize-metadata"];
        if strict {
            args.push("--strict");
        }
        let output = hermit(&args, Some(&guest));
        let label = format!("{args:?}");
        assert_success(&label, &output);
        assert_host_realtime(&label, &output);
    }
}
