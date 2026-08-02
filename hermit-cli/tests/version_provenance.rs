/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[allow(dead_code)]
#[path = "../build_support.rs"]
mod build_support;

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output was not UTF-8")
        .trim()
        .to_owned()
}

fn initialized_repo() -> TempDir {
    let repo = tempfile::tempdir().expect("failed to create temporary repository");
    git(repo.path(), &["init", "--quiet"]);
    fs::write(repo.path().join("tracked.txt"), "clean\n").expect("failed to write fixture");
    git(repo.path(), &["add", "tracked.txt"]);
    git(
        repo.path(),
        &[
            "-c",
            "user.name=Hermit Test",
            "-c",
            "user.email=hermit-test@example.com",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );
    repo
}

#[test]
fn git_watch_paths_resolve_from_a_nested_crate() {
    let repo = initialized_repo();
    let crate_dir = repo.path().join("hermit-cli");
    fs::create_dir(&crate_dir).expect("failed to create nested crate directory");

    let paths = build_support::git_watch_paths_in(&crate_dir);
    let git_dir = repo.path().join(".git");
    let reference = git(repo.path(), &["symbolic-ref", "HEAD"]);

    assert!(paths.contains(&git_dir.join("HEAD")));
    assert!(paths.contains(&git_dir.join("index")));
    assert!(paths.contains(&git_dir.join(reference)));
    assert!(paths.iter().all(|path| path.is_absolute()));
}

#[test]
fn untracked_generated_output_does_not_taint_version() {
    let repo = initialized_repo();
    let expected = git(repo.path(), &["rev-parse", "--short=12", "HEAD"]);

    let output = repo.path().join("ignored/e2e/run/results.jsonl");
    fs::create_dir_all(output.parent().expect("output had no parent"))
        .expect("failed to create generated output directory");
    fs::write(output, "generated\n").expect("failed to write generated output");

    assert_eq!(build_support::git_short_sha_in(repo.path()), expected);
}

#[test]
fn tracked_worktree_and_index_changes_taint_version() {
    let repo = initialized_repo();
    let clean = git(repo.path(), &["rev-parse", "--short=12", "HEAD"]);

    fs::write(repo.path().join("tracked.txt"), "modified\n")
        .expect("failed to modify tracked fixture");
    assert_eq!(
        build_support::git_short_sha_in(repo.path()),
        format!("{clean}-dirty")
    );

    git(repo.path(), &["add", "tracked.txt"]);
    assert_eq!(
        build_support::git_short_sha_in(repo.path()),
        format!("{clean}-dirty")
    );
}
