/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Opt-in that makes a Cargo build embed the source revision. Release builds
/// set it to `1`; regular builds leave it unset.
///
/// A regular build must not depend on Git state: an embedded revision makes
/// every commit, including a documentation-only one, rerun this script and
/// produce a different `hermit` binary, which defeats build and test caching
/// across commits.
pub const STAMP_GIT_SHA_ENV: &str = "HERMIT_STAMP_GIT_SHA";

/// The revision a build embeds when it is not stamped.
/// `src/bin/hermit/version.rs` says `source revision not embedded` for this value.
pub const UNSTAMPED_GIT_SHA: &str = "unknown";

/// The revision to embed for the given `HERMIT_STAMP_GIT_SHA` value: `None`
/// for a regular build, the revision of `root` for a stamped one.
///
/// Only `1` opts in, and unset, empty, or `0` opts out. Any other value is an
/// error so that a misspelled opt-in cannot quietly produce an unstamped
/// release. A stamped build outside a Git checkout is also an error rather
/// than a release that reports `unknown`.
pub fn embedded_git_sha_in(root: &Path, stamp: Option<&str>) -> Result<Option<String>, String> {
    match stamp {
        None | Some("" | "0") => Ok(None),
        Some("1") => {
            let sha = git_short_sha_in(root);
            if sha == UNSTAMPED_GIT_SHA {
                Err(format!(
                    "{STAMP_GIT_SHA_ENV}=1 requires a Git checkout, but `git rev-parse HEAD` failed in {}",
                    root.display()
                ))
            } else {
                Ok(Some(sha))
            }
        }
        Some(other) => Err(format!(
            "{STAMP_GIT_SHA_ENV} must be 1 (stamp the revision) or unset; got {other:?}"
        )),
    }
}

/// Emit `HERMIT_BUILD_GIT_SHA` and the rerun triggers it needs.
///
/// A regular build runs no Git command and registers no Git watch, so moving
/// HEAD or the index cannot rerun the script or change the binary. A stamped
/// build watches the Git metadata that can change the revision or dirty
/// state, as before.
pub fn emit_git_revision() {
    println!("cargo:rerun-if-env-changed={STAMP_GIT_SHA_ENV}");
    let stamp = std::env::var_os(STAMP_GIT_SHA_ENV).map(|value| {
        value
            .into_string()
            .unwrap_or_else(|value| panic!("{STAMP_GIT_SHA_ENV} is not UTF-8: {value:?}"))
    });
    match embedded_git_sha_in(Path::new("."), stamp.as_deref()) {
        Ok(None) => println!("cargo:rustc-env=HERMIT_BUILD_GIT_SHA={UNSTAMPED_GIT_SHA}"),
        Ok(Some(sha)) => {
            println!("cargo:rustc-env=HERMIT_BUILD_GIT_SHA={sha}");
            // Arbitrary tracked worktree files are intentionally not watched:
            // avoiding one Cargo dependency per file keeps incremental builds
            // fast, and staging an edit refreshes the dirty marker through the
            // watched index.
            for path in git_watch_paths() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
        Err(error) => panic!("{error}"),
    }
}

/// Short git revision of the working tree at `root`, with a `-dirty` suffix
/// when tracked or index changes exist. Untracked output does not alter source
/// provenance. Falls back to `unknown` outside a git checkout (for example, a
/// source tarball).
pub fn git_short_sha_in(root: &Path) -> String {
    let Some(sha) = git(root, &["rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".to_owned();
    };
    let dirty = git(root, &["status", "--porcelain", "--untracked-files=no"])
        .map(|status| !status.is_empty())
        .unwrap_or(true);
    if dirty { format!("{sha}-dirty") } else { sha }
}

/// Git metadata that can change the embedded revision or dirty state. Resolve
/// these through Git so this also works from a nested crate and a worktree.
pub fn git_watch_paths() -> Vec<PathBuf> {
    git_watch_paths_in(Path::new("."))
}

pub fn git_watch_paths_in(root: &Path) -> Vec<PathBuf> {
    let mut names = vec![
        "HEAD".to_owned(),
        "index".to_owned(),
        "packed-refs".to_owned(),
    ];
    if let Some(reference) = git(root, &["symbolic-ref", "-q", "HEAD"]) {
        names.push(reference);
    }
    let mut paths: Vec<PathBuf> = names
        .into_iter()
        .filter_map(|name| {
            git(
                root,
                &["rev-parse", "--path-format=absolute", "--git-path", &name],
            )
            .map(PathBuf::from)
            // Cargo treats a missing `rerun-if-changed` path as perpetually
            // dirty. Fresh repositories commonly have no `packed-refs`, so
            // only emit metadata paths that currently exist.
            .filter(|path| path.exists())
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// UTC build date (`YYYY-MM-DD`). Honors `SOURCE_DATE_EPOCH` for reproducible
/// builds, otherwise uses the current wall-clock time.
pub fn build_date() -> String {
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
        .map(|duration| duration.as_secs())
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

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

include!("reverie_pin.rs");

/// The Reverie revision this tree pins, read from the canonical manifest.
///
/// `detcore/Cargo.toml` is the source of truth the pin checker uses; every other
/// site restates it. Returns `unknown` rather than failing the build, because a
/// missing pin must not stop a developer compiling -- the loader treats
/// `unknown` as "cannot verify" and says so instead of asserting a match.
///
/// The crate directory comes from `CARGO_MANIFEST_DIR` as the build script
/// runs, not as it was compiled (`env!`). Checkouts that share a target
/// directory share one compiled build script, so a compile-time path names
/// whichever checkout compiled it: the binary then embeds that checkout's pin,
/// or `unknown` once it is deleted
/// (https://github.com/rrnewton/hermit/issues/3454).
pub fn reverie_pin() -> String {
    match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(crate_dir) => reverie_pin_in(Path::new(&crate_dir)),
        None => "unknown".into(),
    }
}

/// The Reverie revision pinned by the tree containing the crate at `crate_dir`.
pub fn reverie_pin_in(crate_dir: &Path) -> String {
    let Some(root) = crate_dir.parent() else {
        return "unknown".into();
    };
    let Ok(text) = std::fs::read_to_string(root.join("detcore/Cargo.toml")) else {
        return "unknown".into();
    };
    parse_reverie_pin(&text).unwrap_or_else(|| "unknown".into())
}
