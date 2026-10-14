// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! `hermit run --verify-run DIR` twice, then `hermit log-diff --verify-json`
//! over the two directories, against `hermit run --verify --verify-json` on
//! the same guest with the same flags: the two reports must be the same JSON
//! value, and the two commands must exit alike and show the guest's stdout
//! alike.
//!
//! Both sides are given one `HERMIT_EPOCH`. `run --verify` gives its two runs
//! one epoch of its own choosing; two separate runs share one only when the
//! caller gives it, and the comparison refuses runs that did not (tested
//! below).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Output;

use super::append_hermit_args;
use super::append_hermit_args_using_outer_mount;
use super::hermit_run_guard;
use super::stderr;

const EPOCH: &str = "2026-01-01T00:00:00Z";

/// One Hermit invocation: `global` before the subcommand, `run_flags` after
/// `run`, then `-- guest`.
fn hermit_run(global: &[&str], run_flags: &[&str], guest: &[&str], epoch: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args(global).arg("run").args(run_flags);
    let mut rest = vec!["--"];
    rest.extend_from_slice(guest);
    if global.contains(&"--backend=dbt") {
        append_hermit_args_using_outer_mount(&mut command, &rest);
    } else {
        append_hermit_args(&mut command, &rest);
    }
    command
        .env("HERMIT_EPOCH", epoch)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|error| {
            panic!("failed to run hermit {global:?} run {run_flags:?}: {error}")
        })
}

fn log_diff(report: &Path, directories: [&Path; 2], extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hermit"))
        .arg("log-diff")
        .arg("--verify-json")
        .arg(report)
        .args(extra)
        .arg(directories[0])
        .arg(directories[1])
        .output()
        .expect("failed to run hermit log-diff --verify-json")
}

fn report(path: &Path) -> serde_json::Value {
    serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("parsing {}: {error}", path.display()))
}

/// Run `guest` both ways and require the same report and the same exit code.
/// Returns the report.
fn assert_paired_matches_run_verify(
    label: &str,
    global: &[&str],
    flags: &[&str],
    verify_allow: &str,
    guest: &[&str],
) -> serde_json::Value {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let directories = [root.path().join("run1"), root.path().join("run2")];
    for directory in &directories {
        let mut run_flags = flags.to_vec();
        run_flags.extend(["--verify-run", directory.to_str().unwrap()]);
        let run = hermit_run(global, &run_flags, guest, EPOCH);
        assert!(
            directory.join("verify-run.json").exists(),
            "{label}: --verify-run committed no record (exit {:?}): {}",
            run.status.code(),
            stderr(&run)
        );
    }
    let paired_report = root.path().join("paired.json");
    let paired = log_diff(
        &paired_report,
        [&directories[0], &directories[1]],
        &["--verify-allow", verify_allow],
    );

    let internal_report = root.path().join("internal.json");
    let mut run_flags = flags.to_vec();
    run_flags.extend([
        "--verify",
        "--verify-allow",
        verify_allow,
        "--verify-json",
        internal_report.to_str().unwrap(),
    ]);
    let internal = hermit_run(global, &run_flags, guest, EPOCH);

    let (paired_value, internal_value) = (report(&paired_report), report(&internal_report));
    assert_eq!(
        paired_value,
        internal_value,
        "{label}: the paired report differs from run --verify's\nlog-diff stderr:\n{}\nrun \
         --verify stderr:\n{}",
        stderr(&paired),
        stderr(&internal)
    );
    assert_eq!(
        paired.status.code(),
        internal.status.code(),
        "{label}: log-diff exits unlike run --verify\nlog-diff stderr:\n{}\nrun --verify \
         stderr:\n{}",
        stderr(&paired),
        stderr(&internal)
    );
    // The guest's output is shown as `run --verify` shows it.
    assert_eq!(
        String::from_utf8_lossy(&paired.stdout),
        String::from_utf8_lossy(&internal.stdout),
        "{label}: log-diff shows the guest's stdout unlike run --verify"
    );
    paired_value
}

/// A guest that exits 0, and one that exits 3 under each `--verify-allow`
/// that decides it differently.
fn assert_backend_pairs_like_run_verify(label: &str, global: &[&str], flags: &[&str]) {
    let _guard = hermit_run_guard();
    let matched = assert_paired_matches_run_verify(
        &format!("{label} echo"),
        global,
        flags,
        "success",
        &["/bin/echo", "paired"],
    );
    assert_eq!(matched["verdict"], "matched", "{label}: {matched}");
    let failing = ["/bin/sh", "-c", "echo failing; exit 3"];
    let rejected = assert_paired_matches_run_verify(
        &format!("{label} rejected first run"),
        global,
        flags,
        "success",
        &failing,
    );
    assert_eq!(rejected["guest_exit_code"], 3, "{label}: {rejected}");
    let allowed = assert_paired_matches_run_verify(
        &format!("{label} allowed failure"),
        global,
        flags,
        "failure",
        &failing,
    );
    assert_eq!(allowed["verdict"], "matched", "{label}: {allowed}");
}

#[test]
fn paired_verify_runs_report_what_run_verify_reports_on_ptrace() {
    assert_backend_pairs_like_run_verify("ptrace", &[], &["--strict", "--verify-strict"]);
}

#[test]
fn paired_verify_runs_report_what_run_verify_reports_on_dbt() {
    if super::dbt_unavailable("paired_verify_runs_report_what_run_verify_reports_on_dbt") {
        return;
    }
    assert_backend_pairs_like_run_verify("dbt", &["--backend=dbt"], &["--strict"]);
}

#[test]
fn paired_verify_runs_report_what_run_verify_reports_on_kvm() {
    if super::kvm_unavailable("paired_verify_runs_report_what_run_verify_reports_on_kvm") {
        return;
    }
    assert_backend_pairs_like_run_verify("kvm", &["--backend=kvm"], &["--strict"]);
}

#[cfg(feature = "liteinst")]
#[test]
fn paired_verify_runs_report_what_run_verify_reports_in_guest() {
    // The in-guest Tool host cannot deliver the preemption timer yet, and
    // refuses a run with a timeslice.
    for backend in ["liteinst", "in-guest-trap"] {
        let flag = format!("--backend={backend}");
        assert_backend_pairs_like_run_verify(
            backend,
            &[flag.as_str()],
            &["--strict", "--max-timeslice=disabled"],
        );
    }
}

/// The comparison refuses directories whose runs it could not compare
/// faithfully, and finds a difference between two runs that it can.
#[test]
fn paired_verify_runs_refuse_unlike_runs_and_find_a_difference() {
    let _guard = hermit_run_guard();
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let run = |name: &str, flags: &[&str], epoch: &str| {
        let directory = root.path().join(name);
        let mut run_flags = flags.to_vec();
        run_flags.extend(["--verify-run", directory.to_str().unwrap()]);
        let output = hermit_run(&[], &run_flags, &["/bin/echo", "paired"], epoch);
        assert!(output.status.success(), "{name}: {}", stderr(&output));
        directory
    };
    let base = run("base", &["--strict"], EPOCH);
    let other_epoch = run("other-epoch", &["--strict"], "2026-02-02T00:00:00Z");
    let other_comparison = run("other-comparison", &["--strict", "--verify-strict"], EPOCH);
    let same = run("same", &["--strict"], EPOCH);

    let refused = |other: &Path, expected: &str| {
        let path = root.path().join("refused.json");
        let output = log_diff(&path, [&base, other], &[]);
        assert!(!output.status.success(), "{}", stderr(&output));
        assert!(stderr(&output).contains(expected), "{}", stderr(&output));
        assert_eq!(report(&path)["verdict"], "no_result");
    };
    refused(&other_epoch, "different epochs");
    refused(&other_comparison, "were not run alike");
    // A directory without its record did not finish.
    let unfinished = root.path().join("unfinished");
    fs::create_dir(&unfinished).unwrap();
    refused(&unfinished, "did not finish");

    // Alike runs whose guests wrote different output diverge.
    fs::write(same.join("stdout"), b"something else\n").unwrap();
    let path = root.path().join("diverged.json");
    let output = log_diff(&path, [&base, &same], &[]);
    assert!(!output.status.success(), "{}", stderr(&output));
    let diverged = report(&path);
    assert_eq!(diverged["verdict"], "diverged", "{diverged}");
    // The comparison read the directories' logs and left them in place.
    assert!(fs::metadata(base.join("run.log")).unwrap().len() > 0);
    assert!(fs::metadata(same.join("run.log")).unwrap().len() > 0);
}
