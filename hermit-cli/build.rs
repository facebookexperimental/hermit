/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Emits build metadata consumed by `hermit --version`.
//!
//! The crate version is the single source of truth in `Cargo.toml`
//! (`CARGO_PKG_VERSION`); this script only augments it with the build date and
//! the source revision so a released binary can be traced back to a commit.
//! Both values are exposed to the crate through `cargo:rustc-env` and read with
//! `env!` in `src/bin/hermit/version.rs`.
//!
//! Only the Cargo/OSS build runs this script. The fbcode (Buck) build derives
//! its version from `build_info::BuildInfo` instead, so nothing here needs to
//! work under Buck.

use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

fn main() {
    let sha = git_short_sha();
    let date = build_date();

    println!("cargo:rustc-env=HERMIT_BUILD_GIT_SHA={sha}");
    println!("cargo:rustc-env=HERMIT_BUILD_DATE={date}");

    // Re-run when the checked-out revision moves so the embedded SHA stays
    // accurate, and when a reproducible-build timestamp is supplied.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}

/// Short git revision of the working tree, with a `-dirty` suffix when there are
/// uncommitted changes. Falls back to `unknown` outside a git checkout (for
/// example, a source tarball).
fn git_short_sha() -> String {
    let Some(sha) = git(&["rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".to_owned();
    };
    // `git status --porcelain` prints a line per change; any output means dirty.
    let dirty = git(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if dirty { format!("{sha}-dirty") } else { sha }
}

/// UTC build date (`YYYY-MM-DD`). Honors `SOURCE_DATE_EPOCH` for reproducible
/// builds, otherwise uses the current wall-clock time.
fn build_date() -> String {
    let secs = match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(epoch) => epoch.trim().parse::<u64>().unwrap_or_else(|_| now_secs()),
        Err(_) => now_secs(),
    };
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Convert a count of days since the Unix epoch to a civil `(year, month, day)`
/// using Howard Hinnant's `civil_from_days` algorithm. This keeps the script
/// free of a calendar dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}
