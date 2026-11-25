/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The callable form of the x86-64-v3 host-fact refusal predicate
//! for the Buck shell-test runner (`run_probed.py`): given one
//! finished command's exit status and stderr, say whether the run
//! is a decided host-fact refusal. Buck-only (the
//! `host-fact-v3-verdict` target in hermit-cli/BUCK); no Cargo
//! target names this file. The predicate itself stays in
//! `host_fact_v3.rs`, included here as the test harnesses include
//! it, so the classification exists in exactly one place.
//!
//! Usage: `host-fact-v3-verdict EXIT_STATUS`, with the command's
//! stderr on stdin. Exit 0 prints the one `SKIP-ISSUE:` line of a
//! decided host-fact refusal; exit 1 (nothing printed) is every
//! other run, including where the shared environment rule
//! (`env_forbids_skip`) forbids the skip; exit 2 is a usage or
//! read error.

#[allow(dead_code)]
#[path = "host_fact_v3.rs"]
mod host_fact_v3;

use std::io::Read as _;

fn main() {
    std::process::exit(verdict());
}

fn verdict() -> i32 {
    let mut args = std::env::args().skip(1);
    let Some(status) = args.next() else {
        eprintln!("usage: host-fact-v3-verdict EXIT_STATUS");
        return 2;
    };
    if args.next().is_some() {
        eprintln!("usage: host-fact-v3-verdict EXIT_STATUS");
        return 2;
    }
    let Ok(status) = status.parse::<i32>() else {
        eprintln!("host-fact-v3-verdict: EXIT_STATUS is not an integer: {status:?}");
        return 2;
    };
    let mut stderr = Vec::new();
    if std::io::stdin().lock().read_to_end(&mut stderr).is_err() {
        eprintln!("host-fact-v3-verdict: cannot read stdin");
        return 2;
    }
    // The one environment rule both paths share: a required or
    // seam-made refusal is never a skip, whatever its text says.
    // The runner passes the test's environment to this process.
    if host_fact_v3::env_forbids_skip(|key| std::env::var(key).ok()) {
        return 1;
    }
    let stderr = String::from_utf8_lossy(&stderr);
    match host_fact_v3::host_fact_refusal(Some(status), &stderr) {
        Some(refusal) => {
            println!(
                "{}",
                host_fact_v3::skip_issue_reason(host_fact_v3::SKIP_ISSUE_XSAVE, refusal)
            );
            0
        }
        None => 1,
    }
}
