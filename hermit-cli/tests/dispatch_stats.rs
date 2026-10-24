/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The ptrace and e9patch dispatch records, and the DEBUG-only rule.
//!
//! The harness in `common/dispatch_stats.rs` checks what every backend's
//! record must satisfy; the SaBRe case lives in `sabre_examples.rs`, whose
//! validation node stages that backend's artifacts, and the in-guest LiteInst
//! case in the `cli` binary (`common/liteinst_in_guest_programs.rs`). The record
//! must stay out of the INFO log that `--verify` compares.

#[path = "common/dispatch_stats.rs"]
mod dispatch_stats;

use std::path::Path;

use dispatch_stats::GUEST_SYSCALLS;
use dispatch_stats::REPORT_MARKERS;
use dispatch_stats::build_guest;
use dispatch_stats::dispatch_record;
use dispatch_stats::run_guest;

#[test]
fn ptrace_reports_seccomp_stops_and_no_patching() {
    let guest = build_guest("guest", &[]);
    let record = dispatch_record(
        "ptrace",
        Path::new(env!("CARGO_BIN_EXE_hermit")),
        &[],
        &[],
        &guest,
    );
    assert_eq!(record.counters.patched_direct_calls, Some(0), "{record}");
    assert_eq!(record.counters.signal_traps, Some(0), "{record}");
    assert!(
        record.counters.ptrace_seccomp_stops >= Some(GUEST_SYSCALLS),
        "{record}"
    );
    assert_eq!(record.sites, reverie::SiteCounters::NONE_PATCHED);
    assert!(
        record
            .per_process
            .as_ref()
            .is_some_and(|processes| !processes.is_empty()),
        "ptrace attributes its stops per process: {record}"
    );
}

/// The INFO-log check above is only meaningful if the markers it looks for are
/// what the DEBUG report prints.
#[test]
fn debug_log_carries_the_report_without_a_summary_file() {
    let guest = build_guest("guest-debug", &[]);
    let stderr = run_guest(
        "ptrace",
        Path::new(env!("CARGO_BIN_EXE_hermit")),
        "hermit::backend_stats=debug",
        None,
        &[],
        &[],
        &guest,
    );
    for marker in REPORT_MARKERS {
        assert!(
            stderr.contains(marker),
            "the DEBUG report lacks {marker:?}:\n{stderr}"
        );
    }
}

/// No validation node stages e9tool, so this case runs only on request; when
/// it runs, a missing e9tool is a failure, not a skip.
#[test]
#[cfg(feature = "e9patch")]
#[ignore = "needs HERMIT_E9TOOL, which no validation node stages"]
fn e9patch_reports_rewrite_sites_and_tracer_dispatch() {
    assert!(
        std::env::var_os("HERMIT_E9TOOL").is_some(),
        "the e9patch dispatch record needs HERMIT_E9TOOL"
    );
    // e9tool rewrites only the main executable, so the syscall sites must
    // live there.
    let guest = build_guest("guest-static", &["-static"]);
    let record = dispatch_record(
        "e9patch",
        Path::new(env!("CARGO_BIN_EXE_hermit")),
        &[],
        &[],
        &guest,
    );
    let candidates = record.sites.candidates.expect("a static ELF is measured");
    assert!(candidates > 0, "{record}");
    assert_eq!(
        record
            .sites
            .patched
            .zip(record.sites.fell_back)
            .map(|(patched, fell_back)| patched + fell_back),
        Some(candidates),
        "{record}"
    );
}
