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

/// Compile the guest under `CARGO_TARGET_TMPDIR`, which Hermit can see: it
/// isolates the host `/tmp`.
pub(super) fn build_guest(name: &str, extra_flags: &[&str]) -> PathBuf {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dispatch-stats");
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
/// `rust_log` (none when empty) and return the run's stderr.
pub(super) fn run_guest(
    backend: &str,
    hermit: &Path,
    rust_log: &str,
    summary: Option<&Path>,
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
        .args(["run", "--strict", "--base-env=minimal"]);
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

/// Run `guest` under `backend` with a summary JSON and return the record.
pub(super) fn dispatch_record(
    backend: &str,
    hermit: &Path,
    env: &[(&str, &Path)],
    guest: &Path,
) -> DispatchStats {
    let summary_dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("dispatch-stats")
        .join(format!("{backend}-summary"));
    fs::create_dir_all(&summary_dir).expect("failed to create summary directory");
    let summary = summary_dir.join("summary.json");
    let _ = fs::remove_file(&summary);
    let stderr = run_guest(backend, hermit, "", Some(&summary), env, guest);
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
