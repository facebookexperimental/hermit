/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The dispatch-record harness shared by every backend's test binary: a tiny
//! guest that makes a known number of raw `getppid` syscalls, and a runner
//! that returns the record from the run's `--summary-json` after checking the
//! properties every backend must satisfy. Each backend's case lives in the
//! test binary whose validation node stages that backend's artifacts.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use detcore_model::summary::RunSummary;
use detcore_model::syscall_histogram::SyscallHistogramSummary;
use detcore_model::syscall_histogram::SyscallObservationGap;
use detcore_model::syscall_histogram::SyscallObservationScope;
use reverie::DispatchStats;

/// Raw syscalls the guest makes; every backend must dispatch at least these.
pub(super) const GUEST_SYSCALLS: u64 = 64;

const GUEST_SOURCE: &str = r#"
#define _GNU_SOURCE
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    long parent = 0;
    for (int i = 0; i < 64; i++) {
        parent ^= syscall(SYS_getppid);
    }
    printf("dispatch-stats-guest %d\n", parent != -1);
    return 0;
}
"#;

/// A directory under `CARGO_TARGET_TMPDIR` owned by this test process. Nextest
/// runs every test in its own process, so with a shared path two tests running
/// at once would write, or run, the same file.
fn process_build_root() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("dispatch-stats")
        .join("build")
        .join(std::process::id().to_string())
}

/// Compile the guest under `CARGO_TARGET_TMPDIR`, which Hermit can see: it
/// isolates the host `/tmp`.
pub(super) fn build_guest(name: &str, extra_flags: &[&str]) -> PathBuf {
    let build_root = process_build_root();
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    // One source per guest: the tests run in parallel.
    let source = build_root.join(format!("{name}.c"));
    fs::write(&source, GUEST_SOURCE).expect("failed to write guest source");
    let guest = build_root.join(name);
    let output = Command::new("cc")
        .args(["-O1", "-Wall", "-Werror"])
        .args(extra_flags)
        .arg(&source)
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to start the compiler");
    assert!(
        output.status.success(),
        "compiling the dispatch-stats guest failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    guest
}

/// Text only the DEBUG dispatch report contains: its target, message and the
/// record's own rendering.
pub(super) const REPORT_MARKERS: [&str; 3] = [
    "hermit::backend_stats",
    "backend run complete",
    "dispatch stats v",
];

/// Run `guest` under `backend` at `--log=info` plus the `RUST_LOG` directives
/// `rust_log` (none when empty) and return the run's stderr. `run_args` are
/// extra `run` options, placed before the guest.
pub(super) fn run_guest(
    backend: &str,
    hermit: &Path,
    rust_log: &str,
    summary: Option<&Path>,
    run_args: &[&str],
    env: &[(&str, &Path)],
    guest: &Path,
) -> String {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "120s"])
        .env("RUST_LOG", rust_log)
        .arg(hermit)
        .arg("--log=info")
        .arg(format!("--backend={backend}"))
        .args([
            "run",
            "--strict",
            detcore_model::config::CANONICAL_TEST_EPOCH_ARG,
            "--base-env=minimal",
        ])
        .args(run_args);
    if let Some(summary) = summary {
        command.arg(format!("--summary-json={}", summary.display()));
    }
    command.arg("--").arg(guest);
    for (name, value) in env {
        command.env(name, value);
    }
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success() && stdout.contains("dispatch-stats-guest 1"),
        "{rendered} failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    stderr
}

/// Syscalls whose calls may end without returning to Detcore's handler: the
/// thread or image is gone, or the kernel resumes elsewhere.
const UNRETURNING_SYSCALLS: [&str; 5] =
    ["exit", "exit_group", "execve", "execveat", "rt_sigreturn"];

/// The observation scope a `backend` run's histogram must declare: an
/// in-process runtime starts after the guest's dynamic loader and misses its
/// syscalls.
pub(super) fn expected_observation_scope(backend: &str) -> SyscallObservationScope {
    match backend {
        "liteinst" | "in-guest-trap" | "sabre" => {
            SyscallObservationScope::from_gaps([SyscallObservationGap::InProcessRuntimeStart])
        }
        _ => SyscallObservationScope::Complete,
    }
}

/// Check the properties every backend's syscall histogram must have and return
/// it: it is in ascending syscall-number order, its calls sum to the summary's
/// `syscalls`, every call that returns has exactly one outcome, its scope is
/// the backend's, and the guest's own syscalls appear by name. `expected`
/// names syscalls the guest makes at least the given number of times.
pub(super) fn checked_syscall_histogram<'a>(
    backend: &str,
    summary: &'a RunSummary,
    expected: &[(&str, u64)],
) -> &'a SyscallHistogramSummary {
    let histogram = summary
        .syscall_histogram
        .as_ref()
        .unwrap_or_else(|| panic!("{backend}: the summary JSON carries no syscall histogram"));
    assert_eq!(histogram.architecture, "x86_64");
    assert_eq!(
        histogram.observation_scope,
        expected_observation_scope(backend),
        "{backend}: wrong observation scope"
    );
    assert!(
        histogram
            .syscalls
            .windows(2)
            .all(|pair| pair[0].nr < pair[1].nr),
        "{backend}: entries are not in ascending syscall-number order: {histogram:?}"
    );
    assert_eq!(
        summary.syscall_histogram_is_consistent(),
        Some(true),
        "{backend}: the histogram's calls are not the summary's syscalls ({:?}): {histogram:?}",
        summary.syscalls
    );
    for entry in &histogram.syscalls {
        if !UNRETURNING_SYSCALLS.contains(&entry.name.as_str()) {
            assert_eq!(
                entry.counts.unfinished(),
                0,
                "{backend}: {} has calls without an outcome: {:?}",
                entry.name,
                entry.counts
            );
        }
    }
    for (name, at_least) in expected {
        let entry = histogram
            .get(name)
            .unwrap_or_else(|| panic!("{backend}: no {name} in {histogram:?}"));
        assert!(
            entry.counts.calls >= *at_least,
            "{backend}: {name} counted {:?}, expected at least {at_least} calls",
            entry.counts
        );
    }
    histogram
}

/// Run `guest` under `backend` with a summary JSON and return the summary.
/// `run_args` are extra `run` options, as for [`run_guest`].
pub(super) fn guest_summary(
    backend: &str,
    hermit: &Path,
    run_args: &[&str],
    env: &[(&str, &Path)],
    guest: &Path,
) -> RunSummary {
    let summary_dir = process_build_root().join(format!("{backend}-summary"));
    fs::create_dir_all(&summary_dir).expect("failed to create summary directory");
    let summary = summary_dir.join("summary.json");
    let _ = fs::remove_file(&summary);
    let stderr = run_guest(backend, hermit, "", Some(&summary), run_args, env, guest);
    // The record is DEBUG and JSON only: the INFO log that --verify compares
    // and the human summary never see it.
    for marker in REPORT_MARKERS {
        assert!(
            !stderr.contains(marker),
            "{backend}: the INFO log mentions {marker:?}:\n{stderr}"
        );
    }
    let summary: RunSummary = serde_json::from_slice(
        &fs::read(&summary).unwrap_or_else(|error| panic!("{backend}: no summary JSON: {error}")),
    )
    .expect("the summary JSON parses");
    let histogram = checked_syscall_histogram(backend, &summary, &[("getppid", GUEST_SYSCALLS)]);
    assert_eq!(
        histogram.get("getppid").expect("checked").counts.calls,
        GUEST_SYSCALLS,
        "{backend}: the guest makes exactly {GUEST_SYSCALLS} getppid calls"
    );
    summary
}

/// Run `guest` under `backend` with a summary JSON and return the record.
/// `run_args` are extra `run` options, as for [`run_guest`].
pub(super) fn dispatch_record(
    backend: &str,
    hermit: &Path,
    run_args: &[&str],
    env: &[(&str, &Path)],
    guest: &Path,
) -> DispatchStats {
    let summary = guest_summary(backend, hermit, run_args, env, guest);
    let record = summary
        .dispatch_stats
        .unwrap_or_else(|| panic!("{backend}: the summary JSON carries no dispatch record"));
    assert_eq!(
        record.schema_version,
        reverie::DISPATCH_STATS_SCHEMA_VERSION
    );
    assert_eq!(record.backend, backend);
    assert_eq!(record.inconsistencies(), Vec::<String>::new(), "{record}");
    let dispatches = record
        .counters
        .dispatches()
        .unwrap_or_else(|| panic!("{backend}: dispatches are unmeasured: {record}"));
    assert!(
        dispatches >= GUEST_SYSCALLS,
        "{backend}: {dispatches} dispatches for {GUEST_SYSCALLS} guest syscalls: {record}"
    );
    record
}
