/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! An mtime set explicitly through utimensat, futimens or utimes is the mtime
//! the guest's stat reports afterwards
//! (https://github.com/rrnewton/hermit/issues/3565). Before this, Hermit's
//! virtual mtime moved only on writes, so a build unpacked by `tar` saw its
//! files ordered by extraction and `make` re-ran autotools steps that a native
//! build skips.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Output;
use std::time::Duration;
use std::time::SystemTime;

fn command_output(mut command: Command, label: &str) -> Output {
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

#[test]
fn explicit_mtimes_are_reported_by_stat_and_verify_strictly() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("utimensat-mtime");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let guest = build_root.join("utimensat_mtime");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/utimensat_mtime.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "utimensat guest compilation");

    // The guest checks Linux's own behavior, so it must pass without Hermit.
    let native = command_output(Command::new(&guest), "native utimensat guest");
    assert_eq!(
        String::from_utf8_lossy(&native.stdout),
        "explicit mtimes honored\n"
    );

    let report = build_root.join("verify.json");
    let mut run = Command::new("timeout");
    run.args(["--kill-after", "5s", "90s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "--backend=ptrace",
            "run",
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
        ])
        .arg(&report)
        .args(["--base-env=minimal", "--"])
        .arg(&guest);
    let output = command_output(run, "utimensat guest under hermit run --verify-strict");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("explicit mtimes honored"),
        "guest saw a virtual mtime that ignored utimensat\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report).expect("hermit did not write the verification report"),
    )
    .expect("verification report is not JSON");
    assert_eq!(report["bitwise_parity"], serde_json::Value::Bool(true));
    let compared = &report["compared_log_messages"];
    assert!(
        compared["left"].as_u64().is_some_and(|n| n > 0) && compared["left"] == compared["right"],
        "verification compared no INFO messages: {report}"
    );
}

/// One guest thread renames fresh files over the name that another thread's
/// utimensat calls target. The guest reports, for each file that ever held the
/// name, whether it sees the explicit mtime.
///
/// With thread sequentialization no rename runs during a call, so every file the
/// guest sees with the explicit mtime must really have it, and the final call
/// after the renames end must reach the last file.
///
/// Without sequentialization a rename can swap the name away and back during a
/// call, so that Hermit's lookups name a file the kernel did not update. Hermit
/// then leaves every virtual mtime alone, and the guest sees none of them.
/// Before that, the file that took over the name could get the virtual mtime of
/// a call that never touched it.
#[test]
fn a_renamed_over_target_does_not_get_the_explicit_mtime() {
    const EXPLICIT: Duration = Duration::from_secs(1_000_000_000);
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("utimensat-rename-race");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let guest = build_root.join("utimensat_rename_race");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/utimensat_rename_race.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "rename-race guest compilation");

    let fresh_directory = |name: &str| {
        let path = build_root.join(name);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("failed to create the guest's directory");
        path
    };
    // For each keep_N: whether the guest saw the explicit mtime, and whether
    // the file really has it.
    let observations = |directory: &Path, output: &Output| -> Vec<(String, bool, bool)> {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| {
                let (name, seen) = line
                    .split_once(' ')
                    .unwrap_or_else(|| panic!("unexpected guest output line {line:?}"));
                let real = fs::metadata(directory.join(name))
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or_else(|error| panic!("failed to read the mtime of {name}: {error}"))
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("mtime is before 1970");
                (name.to_owned(), seen == "1", real == EXPLICIT)
            })
            .collect()
    };

    // Natively the guest reads the real mtimes, so the check cannot fail.
    let native_directory = fresh_directory("native");
    let mut native = Command::new(&guest);
    native.arg(&native_directory);
    let native = command_output(native, "native rename-race guest");
    let native = observations(&native_directory, &native);
    assert!(!native.is_empty(), "the native guest reported no files");
    assert!(native.iter().all(|(_, seen, real)| seen == real));
    assert!(native.last().is_some_and(|(_, seen, _)| *seen));

    let run = |name: &str, relaxations: &[&str]| {
        let directory = fresh_directory(name);
        let mut run = Command::new("timeout");
        run.args(["--kill-after", "5s", "90s"])
            .arg(hermit_test::hermit_binary())
            .args(["--backend=ptrace", "run"])
            .args(relaxations)
            .args(["--base-env=minimal", "--"])
            .arg(&guest)
            .arg(&directory);
        let output = command_output(run, &format!("rename-race guest under hermit run ({name})"));
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        (observations(&directory, &output), stderr)
    };

    let (observed, stderr) = run("sequentialized", &[]);
    let misattributed: Vec<&str> = observed
        .iter()
        .filter(|(_, seen, real)| *seen && !*real)
        .map(|(name, _, _)| name.as_str())
        .collect();
    assert!(
        misattributed.is_empty(),
        "the guest saw the explicit mtime on {} file(s) the kernel never updated: {misattributed:?}\nstderr:\n{stderr}",
        misattributed.len(),
    );
    // The guest's final call, made after the renames end, must reach the last
    // file, or the run checked nothing.
    assert!(
        observed
            .last()
            .is_some_and(|(_, seen, real)| *seen && *real),
        "the guest's final, unraced call did not show the explicit mtime on the last file: {:?}\nstderr:\n{stderr}",
        observed.last(),
    );

    let (observed, stderr) = run("relaxed", &["--no-sequentialize-threads"]);
    assert!(
        !observed.is_empty(),
        "the relaxed guest reported no files\nstderr:\n{stderr}"
    );
    // The last file really has the explicit mtime, so a run that saw none
    // shows that Hermit, not the race, left the virtual mtimes alone.
    assert!(
        observed.last().is_some_and(|(_, _, real)| *real),
        "the relaxed guest's final call did not reach Linux: {:?}\nstderr:\n{stderr}",
        observed.last(),
    );
    let seen: Vec<&str> = observed
        .iter()
        .filter(|(_, seen, _)| *seen)
        .map(|(name, _, _)| name.as_str())
        .collect();
    assert!(
        seen.is_empty(),
        "without sequentialization the guest saw the explicit mtime on {} file(s): {seen:?}\nstderr:\n{stderr}",
        seen.len(),
    );
}
