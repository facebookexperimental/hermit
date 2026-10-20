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
use std::path::PathBuf;
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
/// `git_short_sha` and `git_watch_paths`, run by `cargo build`; the fixture
/// now reaches them through `emit_git_revision`).
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

fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "."]);
    git(
        repo,
        &[
            "-c",
            "user.name=Hermit Test",
            "-c",
            "user.email=hermit-test@example.com",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
}

/// A crate inside `repo` whose build script is `build_support::emit_git_revision`
/// and whose binary prints the revision it embedded.
fn provenance_fixture(repo: &Path) -> PathBuf {
    let crate_dir = repo.join("fixture");
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
    let gen = build_support::build_date();
    println!("cargo:rustc-env=FIXTURE_DATE={gen}");
    build_support::emit_git_revision();
}
"#,
    )
    .expect("failed to write fixture build script");
    fs::write(
        crate_dir.join("src/main.rs"),
        "fn main() { println!(\"{}\", env!(\"HERMIT_BUILD_GIT_SHA\")); }\n",
    )
    .expect("failed to write fixture binary");
    fs::write(crate_dir.join(".gitignore"), "/target\n").expect("failed to write .gitignore");
    commit_all(repo, "add fixture crate");
    crate_dir
}

/// One fixture build with `HERMIT_STAMP_GIT_SHA` set to `stamp`, or removed.
fn fixture_build(crate_dir: &Path, stamp: Option<&str>) -> std::process::Output {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(crate_dir)
        .env("CARGO_TARGET_DIR", crate_dir.join("target"))
        .args(["build", "--message-format=json"]);
    match stamp {
        Some(value) => command.env(build_support::STAMP_GIT_SHA_ENV, value),
        None => command.env_remove(build_support::STAMP_GIT_SHA_ENV),
    };
    command.output().expect("failed to run cargo build")
}

/// Build the fixture and return Cargo's JSON messages.
fn checked_fixture_build(crate_dir: &Path, stamp: Option<&str>) -> Vec<serde_json::Value> {
    let output = fixture_build(crate_dir, stamp);
    assert!(
        output.status.success(),
        "fixture build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("cargo output was not UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("cargo message was not JSON"))
        .collect()
}

/// Whether Cargo reused the fixture binary without rebuilding it.
fn fixture_binary_was_fresh(messages: &[serde_json::Value]) -> bool {
    let artifact = messages
        .iter()
        .find(|message| {
            message["reason"] == "compiler-artifact"
                && message["target"]["name"] == "provenance-fixture"
                && message["target"]["kind"][0] == "bin"
        })
        .expect("cargo reported no fixture binary artifact");
    artifact["fresh"]
        .as_bool()
        .expect("compiler-artifact had no fresh flag")
}

/// The `cargo:` lines the fixture's build script printed.
fn build_script_output(messages: &[serde_json::Value]) -> String {
    let out_dir = messages
        .iter()
        .find(|message| message["reason"] == "build-script-executed")
        .and_then(|message| message["out_dir"].as_str())
        .expect("cargo reported no build-script execution");
    let output = Path::new(out_dir)
        .parent()
        .expect("build-script out_dir had no parent")
        .join("output");
    fs::read_to_string(&output).expect("failed to read build-script output")
}

#[test]
fn regular_cargo_build_embeds_no_revision_and_ignores_git_state() {
    let repo = initialized_repo();
    let crate_dir = provenance_fixture(repo.path());
    let binary = crate_dir.join("target/debug/provenance-fixture");

    let first = checked_fixture_build(&crate_dir, None);
    assert_eq!(checked_output(&mut Command::new(&binary)), "unknown");
    let script_output = build_script_output(&first);
    assert!(
        script_output.contains("cargo:rustc-env=HERMIT_BUILD_GIT_SHA=unknown"),
        "{script_output}"
    );
    assert!(
        !script_output.contains("rerun-if-changed"),
        "a regular build must not watch Git metadata: {script_output}"
    );
    let built = fs::read(&binary).expect("failed to read fixture binary");

    // A new commit and a staged edit outside the crate move HEAD, the branch,
    // and the index. None of them may rebuild or change the binary.
    fs::write(repo.path().join("tracked.txt"), "documentation only\n")
        .expect("failed to modify tracked fixture");
    commit_all(repo.path(), "documentation-only change");
    fs::write(repo.path().join("tracked.txt"), "staged\n")
        .expect("failed to modify tracked fixture");
    git(repo.path(), &["add", "tracked.txt"]);

    let second = checked_fixture_build(&crate_dir, None);
    assert!(
        fixture_binary_was_fresh(&second),
        "a commit outside the crate rebuilt a regular build"
    );
    assert_eq!(
        fs::read(&binary).expect("failed to read fixture binary"),
        built,
        "a commit outside the crate changed a regular build's binary"
    );

    // `0` and the empty string are spelled-out opt-outs, not stamps.
    for opt_out in ["0", ""] {
        checked_fixture_build(&crate_dir, Some(opt_out));
        assert_eq!(checked_output(&mut Command::new(&binary)), "unknown");
    }
}

#[test]
fn cargo_rebuilds_provenance_after_staging_a_tracked_edit() {
    let repo = initialized_repo();
    let crate_dir = provenance_fixture(repo.path());
    let binary = crate_dir.join("target/debug/provenance-fixture");

    checked_fixture_build(&crate_dir, None);
    assert_eq!(checked_output(&mut Command::new(&binary)), "unknown");

    // Setting the opt-in alone must rerun the build script.
    let expected = git(repo.path(), &["rev-parse", "--short=12", "HEAD"]);
    checked_fixture_build(&crate_dir, Some("1"));
    assert_eq!(checked_output(&mut Command::new(&binary)), expected);

    fs::write(repo.path().join("tracked.txt"), "modified\n")
        .expect("failed to modify tracked fixture");
    checked_fixture_build(&crate_dir, Some("1"));
    assert_eq!(
        checked_output(&mut Command::new(&binary)),
        expected,
        "an unstaged edit intentionally leaves embedded provenance unchanged"
    );

    git(repo.path(), &["add", "tracked.txt"]);
    checked_fixture_build(&crate_dir, Some("1"));
    assert_eq!(
        checked_output(&mut Command::new(&binary)),
        format!("{expected}-dirty"),
        "staging changes the watched index and must refresh embedded provenance"
    );

    commit_all(repo.path(), "commit the staged edit");
    let committed = git(repo.path(), &["rev-parse", "--short=12", "HEAD"]);
    checked_fixture_build(&crate_dir, Some("1"));
    assert_eq!(
        checked_output(&mut Command::new(&binary)),
        committed,
        "a new commit must refresh a stamped build's revision"
    );

    let misspelled = fixture_build(&crate_dir, Some("true"));
    assert!(
        !misspelled.status.success(),
        "a misspelled opt-in must fail the build rather than leave it unstamped"
    );
    assert!(
        String::from_utf8_lossy(&misspelled.stderr)
            .contains("HERMIT_STAMP_GIT_SHA must be 1 (stamp the revision) or unset"),
        "{}",
        String::from_utf8_lossy(&misspelled.stderr)
    );

    let manifest = crate_dir.join("Cargo.toml");
    let rust_2021 = fs::read_to_string(&manifest).expect("failed to read fixture manifest");
    fs::write(&manifest, rust_2021.replace("\"2021\"", "\"2024\""))
        .expect("failed to plant edition mismatch");
    let mismatched = fixture_build(&crate_dir, Some("1"));
    assert!(
        !mismatched.status.success(),
        "the Rust-2021 fixture must reject a planted Rust-2024 edition mismatch"
    );
    // With `--message-format=json`, rustc's diagnostics arrive on stdout.
    let diagnostics = String::from_utf8_lossy(&mismatched.stdout);
    assert!(
        diagnostics.contains("expected identifier, found reserved keyword `gen`"),
        "the mismatch must fail on the edition sentinel: {diagnostics}"
    );
}

#[test]
fn only_an_explicit_opt_in_embeds_a_revision() {
    let repo = initialized_repo();
    let outside = tempfile::tempdir().expect("failed to create non-repository directory");
    let expected = git(repo.path(), &["rev-parse", "--short=12", "HEAD"]);

    for opt_out in [None, Some(""), Some("0")] {
        assert_eq!(
            build_support::embedded_git_sha_in(repo.path(), opt_out),
            Ok(None)
        );
        assert_eq!(
            build_support::embedded_git_sha_in(outside.path(), opt_out),
            Ok(None),
            "an unstamped build must not need a Git checkout"
        );
    }
    assert_eq!(
        build_support::embedded_git_sha_in(repo.path(), Some("1")),
        Ok(Some(expected))
    );
    let error = build_support::embedded_git_sha_in(outside.path(), Some("1"))
        .expect_err("a stamped build outside a checkout must fail");
    assert!(error.contains("requires a Git checkout"), "{error}");
    for misspelled in ["true", "yes", " 1"] {
        let error = build_support::embedded_git_sha_in(repo.path(), Some(misspelled))
            .expect_err("only 1 opts in");
        assert!(error.contains("must be 1"), "{error}");
    }
}

fn write_reverie_pin(tree: &Path, rev: &str) {
    fs::write(
        tree.join("detcore/Cargo.toml"),
        format!(
            "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie\", rev = \"{rev}\" }}\n"
        ),
    )
    .expect("failed to write fixture pin");
}

/// Two checkouts that share a target directory share hermit-cli's compiled
/// build script. The pin it embeds must still be the pin of the checkout being
/// built (https://github.com/rrnewton/hermit/issues/3454).
///
/// The second checkout is a `cp -a` copy, so its build script is no newer than
/// the compiled one and Cargo reuses it; the changed pin manifest only makes
/// Cargo rerun it. Measured with `reverie_pin` reading `env!("CARGO_MANIFEST_DIR")`
/// as before the fix: the second build embedded the first checkout's pin.
#[test]
fn a_build_script_shared_through_a_target_dir_embeds_the_building_trees_pin() {
    // No git runs here, but the call keeps this binary's environment removal
    // ahead of every spawned process (see without_inherited_repository_location).
    without_inherited_repository_location();
    const FIRST: &str = "1111111111111111111111111111111111111111";
    const SECOND: &str = "2222222222222222222222222222222222222222";
    const THIRD: &str = "3333333333333333333333333333333333333333";

    let scratch = tempfile::tempdir().expect("failed to create scratch directory");
    let first = scratch.path().join("first");
    let crate_dir = first.join("hermit-cli");
    fs::create_dir_all(crate_dir.join("src")).expect("failed to create fixture crate");
    fs::create_dir_all(first.join("detcore")).expect("failed to create fixture detcore");
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
name = "reverie-pin-fixture"
version = "0.0.0"
edition = "2021"
build = "build.rs"
"#,
    )
    .expect("failed to write fixture manifest");
    // The same two lines as hermit-cli/build.rs.
    fs::write(
        crate_dir.join("build.rs"),
        r#"#[allow(dead_code)]
#[path = "build_support.rs"]
mod build_support;

fn main() {
    let reverie_pin = build_support::reverie_pin();
    println!("cargo:rustc-env=FIXTURE_REVERIE_PIN={reverie_pin}");
    println!("cargo:rerun-if-changed=../detcore/Cargo.toml");
}
"#,
    )
    .expect("failed to write fixture build script");
    fs::write(
        crate_dir.join("src/main.rs"),
        "fn main() { println!(\"{}\", env!(\"FIXTURE_REVERIE_PIN\")); }\n",
    )
    .expect("failed to write fixture binary");
    write_reverie_pin(&first, FIRST);

    let target = scratch.path().join("target");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let build_and_run = |tree: &Path| {
        checked_output(
            Command::new(&cargo)
                .current_dir(tree.join("hermit-cli"))
                .env("CARGO_TARGET_DIR", &target)
                .args(["build", "--quiet"]),
        );
        checked_output(&mut Command::new(target.join("debug/reverie-pin-fixture")))
    };
    assert_eq!(build_and_run(&first), FIRST);

    let second = scratch.path().join("second");
    checked_output(Command::new("cp").arg("-a").arg(&first).arg(&second));
    write_reverie_pin(&second, SECOND);
    assert_eq!(
        build_and_run(&second),
        SECOND,
        "the second checkout's build embedded another checkout's pin"
    );

    fs::remove_dir_all(&first).expect("failed to delete the first checkout");
    write_reverie_pin(&second, THIRD);
    assert_eq!(
        build_and_run(&second),
        THIRD,
        "the build read the deleted first checkout's pin manifest"
    );
}
