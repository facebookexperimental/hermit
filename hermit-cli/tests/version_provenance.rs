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

/// Remove Git's repository-location variables from this test process.
///
/// Git exports them to hooks and `git rebase --exec` steps, and they override
/// the working directory. These tests name every repository by directory,
/// including through the in-process `build_support` calls and the fixture
/// `cargo build`, so an inherited `GIT_DIR` would point the fixture's
/// `git init` and commits at the caller's repository
/// (https://github.com/rrnewton/hermit/issues/3362).
///
/// The removal is process-wide rather than per command because the
/// in-process `build_support` calls and the fixture's build script build their
/// own `Command`s. Every test here depends on it. Measured 2026-09-29 with the
/// removal disabled, each test run alone under an inherited `GIT_DIR`: all
/// four wrote into the caller's repository, and three failed:
/// git_watch_paths_resolve_from_a_nested_crate (`git_watch_paths_in`),
/// untracked_generated_output_does_not_taint_version (`git_short_sha_in`), and
/// cargo_rebuilds_provenance_after_staging_a_tracked_edit (the fixture crate's
/// `git_short_sha` and `git_watch_paths`, run by `cargo build`).
/// tracked_worktree_and_index_changes_taint_version (`git_short_sha_in`) still
/// passed, because its fixture and its reads followed the caller together.
///
/// It happens once, at the first call. Every test in this binary makes that
/// call first, through `initialized_repo`, so no test here runs git with the
/// inherited variables, whatever order libtest schedules them in. A new test
/// that runs git without `initialized_repo` would not have that guarantee.
fn without_inherited_repository_location() {
    static REMOVE: std::sync::Once = std::sync::Once::new();
    REMOVE.call_once(|| {
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
        ] {
            // SAFETY: every test in this binary calls this through
            // `initialized_repo` before it spawns a process, and `Once`
            // holds every other caller until the removal finishes, so no
            // thread reads the environment while it changes.
            unsafe { std::env::remove_var(name) };
        }
    });
}

fn initialized_repo() -> TempDir {
    without_inherited_repository_location();
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
    let packed_refs = git_dir.join("packed-refs");

    assert!(paths.contains(&git_dir.join("HEAD")));
    assert!(paths.contains(&git_dir.join("index")));
    assert!(paths.contains(&git_dir.join(reference)));
    assert!(!packed_refs.exists());
    assert!(!paths.contains(&packed_refs));
    assert!(!paths.contains(&repo.path().join("tracked.txt")));
    assert!(paths.iter().all(|path| path.is_absolute()));
    assert!(paths.iter().all(|path| path.exists()));
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
fn cargo_rebuilds_provenance_after_staging_a_tracked_edit() {
    let repo = initialized_repo();
    let crate_dir = repo.path().join("fixture");
    fs::create_dir_all(crate_dir.join("src")).expect("failed to create fixture crate");
    for source in ["build_support.rs", "reverie_pin.rs"] {
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(source),
            crate_dir.join(source),
        )
        .expect("failed to copy build support source");
    }
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
    // `gen` is legal in Rust 2021 and reserved in Rust 2024. Keep the fixture's
    // edition observable so changing it cannot conceal incompatible support.
    let gen = build_support::git_short_sha();
    println!("cargo:rustc-env=FIXTURE_SHA={gen}");
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
        expected,
        "an unstaged edit intentionally leaves embedded provenance unchanged"
    );

    git(repo.path(), &["add", "tracked.txt"]);
    checked_output(
        Command::new(&cargo)
            .current_dir(&crate_dir)
            .args(["build", "--quiet"]),
    );
    assert_eq!(
        checked_output(&mut Command::new(&binary)),
        format!("{expected}-dirty"),
        "staging changes the watched index and must refresh embedded provenance"
    );

    let manifest = crate_dir.join("Cargo.toml");
    let rust_2021 = fs::read_to_string(&manifest).expect("failed to read fixture manifest");
    fs::write(&manifest, rust_2021.replace("\"2021\"", "\"2024\""))
        .expect("failed to plant edition mismatch");
    let mismatched = Command::new(&cargo)
        .current_dir(&crate_dir)
        .args(["build", "--quiet"])
        .output()
        .expect("failed to build mismatched fixture");
    assert!(
        !mismatched.status.success(),
        "the Rust-2021 fixture must reject a planted Rust-2024 edition mismatch"
    );
    assert!(
        String::from_utf8_lossy(&mismatched.stderr)
            .contains("expected identifier, found reserved keyword `gen`"),
        "the mismatch must fail on the edition sentinel: {}",
        String::from_utf8_lossy(&mismatched.stderr)
    );
}
