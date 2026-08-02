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

fn checked_output(command: &mut Command) -> String {
    let output = command.output().expect("failed to run command");
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("command output was not UTF-8")
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
    assert!(paths.contains(&repo.path().join("tracked.txt")));
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

#[test]
fn cargo_rebuilds_provenance_after_an_unstaged_tracked_edit() {
    let repo = initialized_repo();
    let crate_dir = repo.path().join("fixture");
    fs::create_dir_all(crate_dir.join("src")).expect("failed to create fixture crate");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("build_support.rs"),
        crate_dir.join("build_support.rs"),
    )
    .expect("failed to copy build support");
    fs::write(
        crate_dir.join("Cargo.toml"),
        r#"[package]
name = "provenance-fixture"
version = "0.0.0"
edition = "2021"
build = "build.rs"
"#,
    )
    .expect("failed to write fixture manifest");
    fs::write(
        crate_dir.join("build.rs"),
        r#"#[path = "build_support.rs"]
mod build_support;

fn main() {
    println!("cargo:rustc-env=FIXTURE_SHA={}", build_support::git_short_sha());
    for path in build_support::git_watch_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
"#,
    )
    .expect("failed to write fixture build script");
    fs::write(
        crate_dir.join("src/main.rs"),
        "fn main() { println!(\"{}\", env!(\"FIXTURE_SHA\")); }\n",
    )
    .expect("failed to write fixture binary");
    git(repo.path(), &["add", "."]);
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
            "add fixture crate",
        ],
    );

    let expected = git(repo.path(), &["rev-parse", "--short=12", "HEAD"]);
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    checked_output(
        Command::new(&cargo)
            .current_dir(&crate_dir)
            .args(["build", "--quiet"]),
    );
    let binary = crate_dir.join("target/debug/provenance-fixture");
    assert_eq!(checked_output(&mut Command::new(&binary)), expected);

    fs::write(repo.path().join("tracked.txt"), "modified\n")
        .expect("failed to modify tracked fixture");
    checked_output(
        Command::new(&cargo)
            .current_dir(&crate_dir)
            .args(["build", "--quiet"]),
    );
    assert_eq!(
        checked_output(&mut Command::new(&binary)),
        format!("{expected}-dirty")
    );
}
