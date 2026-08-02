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

/// Short git revision of the working tree, with a `-dirty` suffix when tracked
/// or index changes exist. Untracked output does not alter source provenance.
/// Falls back to `unknown` outside a git checkout (for example, a source
/// tarball).
pub fn git_short_sha() -> String {
    git_short_sha_in(Path::new("."))
}

pub fn git_short_sha_in(root: &Path) -> String {
    let Some(sha) = git(root, &["rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".to_owned();
    };
    let dirty = git(root, &["status", "--porcelain", "--untracked-files=no"])
        .map(|status| !status.is_empty())
        .unwrap_or(true);
    if dirty { format!("{sha}-dirty") } else { sha }
}

/// Git metadata and tracked source files that can change the embedded revision
/// or dirty state. Resolve these through Git so this also works from a nested
/// crate and a worktree.
pub fn git_watch_paths() -> Vec<PathBuf> {
    git_watch_paths_in(Path::new("."))
}

pub fn git_watch_paths_in(root: &Path) -> Vec<PathBuf> {
    let repository = git(root, &["rev-parse", "--show-toplevel"]).map(PathBuf::from);
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
        })
        .collect();
    if let Some(repository) = repository
        && let Some(tracked) = git(&repository, &["ls-files", "--full-name"])
    {
        paths.extend(
            tracked
                .lines()
                .filter(|path| !path.is_empty())
                .map(|path| repository.join(path)),
        );
    }
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
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}
