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
    super::hermit_test::hermit_output(
        &format!("hermit {global:?} run {run_flags:?}"),
        &mut hermit_run_command(global, run_flags, guest, epoch),
    )
}

fn hermit_run_command(global: &[&str], run_flags: &[&str], guest: &[&str], epoch: &str) -> Command {
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
        .stdin(std::process::Stdio::null());
    command
}

fn log_diff(report: &Path, directories: [&Path; 2], extra: &[&str]) -> Output {
    // Offline comparison borrows its inputs, including on refusal and
    // divergence. A saved run may be compared again after this command.
    let original_logs = directories.map(|directory| {
        let path = directory.join("run.log");
        path.exists().then(|| fs::read(path).unwrap())
    });
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .arg("log-diff")
        .arg("--verify-json")
        .arg(report)
        .args(extra)
        .arg(directories[0])
        .arg(directories[1])
        .output()
        .expect("failed to run hermit log-diff --verify-json");
    for (directory, original) in directories.into_iter().zip(original_logs) {
        if let Some(original) = original {
            assert_eq!(
                fs::read(directory.join("run.log")).unwrap(),
                original,
                "offline comparison changed the caller's log in {}",
                directory.display()
            );
        }
    }
    output
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

#[cfg(feature = "dbt")]
#[test]
fn dbt_stats_paths_are_private_in_plain_live_and_saved_runs() {
    let _guard = hermit_run_guard();
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let run = |extra_flags: &[&str], guest: &[&str]| {
        let mut flags = ["--strict", "--base-env=minimal"]
            .into_iter()
            .collect::<Vec<_>>();
        // Keep the old random host sink under /tmp, where the container has to
        // bind it. An enclosing test's TMPDIR must not bypass this regression.
        flags.extend_from_slice(extra_flags);
        super::hermit_test::hermit_output(
            "DBT stats path regression",
            hermit_run_command(&["--log=info", "--backend=dbt"], &flags, guest, EPOCH)
                .env("TMPDIR", "/tmp"),
        )
    };
    let assert_mountinfo = |output: &[u8]| {
        let text = std::str::from_utf8(output).unwrap();
        assert!(!text.is_empty());
        assert!(
            !text.contains("hermit-dbt-verify-stats-"),
            "host stats path leaked"
        );
        assert!(
            text.lines().any(|line| {
                let fields = line.split_whitespace().collect::<Vec<_>>();
                fields.len() > 5
                    && fields[3] == "/tmp/.hermit-dbt-stats-0"
                    && fields[4] == "/tmp/.hermit-dbt-stats-0"
            }),
            "the real stats bind is missing its declared root and mountpoint"
        );
    };
    let assert_matched = |path: &Path, output: &Output| {
        let value = super::read_terminal_dbt_verdict(path);
        assert_eq!(
            value["verdict"],
            "matched",
            "{value}\nfull guest stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            stderr(output)
        );
        assert_eq!(value["verified"], true, "{value}");
        assert_eq!(value["bitwise_parity"], true, "{value}");
        assert_eq!(value["comparison"]["compare_io_buffers"], true, "{value}");
        assert_eq!(value["comparison"]["full_trace"], true, "{value}");
        assert_eq!(
            value["compared_log_messages"]["left"],
            value["compared_log_messages"]["right"]
        );
        value
    };
    let guest = ["/bin/cat", "/proc/self/mountinfo"];

    // Run the unchanged full strict oracle first: the old mapping fails with
    // an actual payload divergence, while both physical guests exit normally.
    let live_report = root.path().join("live.json");
    let live = run(
        &[
            "--verify",
            "--verify-strict",
            "--verify-json",
            live_report.to_str().unwrap(),
        ],
        &guest,
    );
    let live_value = assert_matched(&live_report, &live);
    assert!(live.status.success(), "{}", stderr(&live));
    assert_mountinfo(&live.stdout);

    let plain_report = root.path().join("plain.json");
    let plain = run(
        &["--backend-engagement-json", plain_report.to_str().unwrap()],
        &guest,
    );
    assert!(plain.status.success(), "{}", stderr(&plain));
    let engagement = report(&plain_report);
    assert_eq!(engagement["engagement"]["backend"], "dbt");
    assert!(
        engagement["engagement"]["counted_branches"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert_mountinfo(&plain.stdout);

    let directories = [root.path().join("run1"), root.path().join("run2")];
    for directory in &directories {
        let saved = run(
            &[
                "--verify-strict",
                "--verify-run",
                directory.to_str().unwrap(),
            ],
            &guest,
        );
        assert!(saved.status.success(), "{}", stderr(&saved));
        // --verify-run stores the guest output; log-diff publishes it after
        // comparing the two complete saved runs.
        assert!(saved.stdout.is_empty());
        let saved_stdout = fs::read(directory.join("stdout")).unwrap();
        assert_mountinfo(&saved_stdout);
        assert_eq!(saved_stdout, live.stdout);
        let stats =
            reverie_dbt::backend_stats::DbtStatsCapture::read(&directory.join("dbt-stats.bin"))
                .snapshot()
                .unwrap();
        assert!(stats.process_images() > 0);
        assert!(stats.counted_branches() > 0);
        assert!(stats.intercepted_syscalls() > 0);
    }
    let paired_report = root.path().join("paired.json");
    let paired = log_diff(
        &paired_report,
        [&directories[0], &directories[1]],
        &["--verify-allow", "success"],
    );
    assert!(paired.status.success(), "{}", stderr(&paired));
    assert_eq!(assert_matched(&paired_report, &paired), live_value);
    assert_eq!(paired.stdout, live.stdout);

    let uncontained_report = root.path().join("uncontained.json");
    // Host namespace mode intentionally cannot claim strict determinism.
    // This checks only that its original host-path capture still works.
    let uncontained = super::hermit_test::hermit_output(
        "DBT stats without a namespace",
        hermit_run_command(
            &["--log=info", "--backend=dbt"],
            &[
                "--base-env=minimal",
                "--no-namespace",
                "--backend-engagement-json",
                uncontained_report.to_str().unwrap(),
            ],
            &["/bin/true"],
            EPOCH,
        )
        .env("TMPDIR", "/tmp"),
    );
    assert!(uncontained.status.success(), "{}", stderr(&uncontained));
    let engagement = report(&uncontained_report);
    assert_eq!(engagement["engagement"]["backend"], "dbt");
    assert!(
        engagement["engagement"]["counted_branches"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
}

#[cfg(feature = "dbt")]
#[test]
fn dbt_stats_bind_preserves_supplied_tmp_contents() {
    let _guard = hermit_run_guard();
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let backing = root.path().join("tmp");
    fs::create_dir(&backing).unwrap();
    let target = backing.join(".hermit-dbt-stats-0");
    let run = |guest: &[&str]| {
        super::hermit_test::hermit_output(
            "DBT stats bind with supplied /tmp",
            hermit_run_command(
                &["--backend=dbt"],
                &["--strict", "--tmp", backing.to_str().unwrap()],
                guest,
                EPOCH,
            )
            .env("TMPDIR", "/tmp"),
        )
    };
    let created = run(&["/bin/true"]);
    assert!(created.status.success(), "{}", stderr(&created));
    assert!(target.is_dir());
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);

    let sentinel = target.join("user-data");
    fs::write(&sentinel, b"preserve these bytes").unwrap();
    let covered = run(&[
        "/bin/sh",
        "-c",
        "test -d /tmp/.hermit-dbt-stats-0 && test ! -e /tmp/.hermit-dbt-stats-0/user-data",
    ]);
    assert!(covered.status.success(), "{}", stderr(&covered));
    assert_eq!(fs::read(&sentinel).unwrap(), b"preserve these bytes");

    fs::remove_file(sentinel).unwrap();
    fs::remove_dir(&target).unwrap();
    fs::write(&target, b"an incompatible user file").unwrap();
    let refused = run(&["/bin/true"]);
    assert!(
        !refused.status.success(),
        "file target was silently replaced"
    );
    assert_eq!(fs::read(target).unwrap(), b"an incompatible user file");
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

/// `log-diff --verify-json --verify-log-dir` keeps the compared logs as `run
/// --verify --keep-logs --verify-log-dir` keeps them: run 1's alone after a
/// match, both after a divergence, as links that leave the directories whole.
/// And where `run --verify` would not have started run 2 (run 1's status is
/// not allowed), the comparison needs no run 2 directory.
#[test]
fn paired_verify_runs_keep_logs_and_need_no_run_two_after_a_rejected_run_one() {
    let _guard = hermit_run_guard();
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let run = |name: &str, guest: &[&str]| {
        let directory = root.path().join(name);
        let output = hermit_run(
            &[],
            &["--strict", "--verify-run", directory.to_str().unwrap()],
            guest,
            EPOCH,
        );
        assert!(
            directory.join("verify-run.json").exists(),
            "{name}: {}",
            stderr(&output)
        );
        directory
    };
    let retained = |logs: &Path| {
        let mut names = fs::read_dir(logs)
            .unwrap()
            .map(|entry| {
                let name = entry.unwrap().file_name().into_string().unwrap();
                name[..name.find("_log_").unwrap()].to_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let echo = ["/bin/echo", "paired"];
    let (first, second) = (run("first", &echo), run("second", &echo));

    let logs = root.path().join("matched-logs");
    let output = log_diff(
        &root.path().join("matched.json"),
        [&first, &second],
        &["--verify-log-dir", logs.to_str().unwrap()],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(retained(&logs), ["run1"]);
    assert!(first.join("run.log").exists() && second.join("run.log").exists());

    fs::write(second.join("stdout"), b"something else\n").unwrap();
    let logs = root.path().join("diverged-logs");
    let output = log_diff(
        &root.path().join("diverged.json"),
        [&first, &second],
        &["--verify-log-dir", logs.to_str().unwrap()],
    );
    assert!(!output.status.success());
    assert_eq!(retained(&logs), ["run1", "run2"]);

    let failing = run("failing", &["/bin/sh", "-c", "exit 3"]);
    let logs = root.path().join("rejected-logs");
    let verdict = root.path().join("rejected.json");
    let output = log_diff(
        &verdict,
        [&failing, &root.path().join("never-run")],
        &["--verify-log-dir", logs.to_str().unwrap()],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("First run errored during --verify"),
        "{}",
        stderr(&output)
    );
    assert_eq!(report(&verdict)["guest_exit_code"], 3);
    assert_eq!(retained(&logs), ["run1"]);
}

/// `hermit record start --verify-run`, then the same with
/// `--verify-replay-of` the first, then `log-diff --verify-json`, against
/// `record start --verify --verify-json`: the same report, exit code and
/// guest stdout, for a guest that succeeds and one that fails (which `record
/// start --verify` replays and compares all the same).
#[test]
fn paired_recording_and_replay_report_what_record_verify_reports() {
    let _guard = hermit_run_guard();
    let record = |flags: &[&str], guest: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        super::hermit_test::hermit_output(
            "hermit run",
            command
                .args(["record", "start", "--strict", "--verify-strict"])
                .args(flags)
                .arg("--")
                .args(guest)
                .stdin(std::process::Stdio::null()),
        )
    };
    for (label, guest) in [
        ("echo", &["/bin/echo", "recorded"][..]),
        ("failing", &["/bin/sh", "-c", "echo failing; exit 3"][..]),
    ] {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let (first, second) = (root.path().join("record"), root.path().join("replay"));
        let recorded = record(&["--verify-run", first.to_str().unwrap()], guest);
        assert!(
            first.join("verify-run.json").exists(),
            "{label}: {}",
            stderr(&recorded)
        );
        let replayed = record(
            &[
                "--verify-run",
                second.to_str().unwrap(),
                "--verify-replay-of",
                first.to_str().unwrap(),
            ],
            guest,
        );
        assert!(
            second.join("verify-run.json").exists(),
            "{label}: {}",
            stderr(&replayed)
        );
        let paired_report = root.path().join("paired.json");
        let paired = log_diff(&paired_report, [&first, &second], &[]);
        let internal_report = root.path().join("internal.json");
        let internal = record(
            &[
                "--verify",
                "--verify-json",
                internal_report.to_str().unwrap(),
            ],
            guest,
        );
        let paired_value = report(&paired_report);
        assert_eq!(
            paired_value,
            report(&internal_report),
            "{label}\nlog-diff stderr:\n{}\nrecord --verify stderr:\n{}",
            stderr(&paired),
            stderr(&internal)
        );
        assert_eq!(paired_value["verdict"], "matched", "{label}");
        assert_eq!(paired.status.code(), internal.status.code(), "{label}");
        assert_eq!(
            String::from_utf8_lossy(&paired.stdout),
            String::from_utf8_lossy(&internal.stdout),
            "{label}"
        );
    }
}
