/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Integration coverage for `hermit analyze`, the root-cause search that locates
//! the racing instructions behind a nondeterministic failure.
//!
//! These mirror the Buck `analyze_*` targets from `tests/BUCK`, which drive
//! `tests/util/hermit_analyze_test.sh`: each scenario runs `hermit analyze
//! --search` over a chaotic schedule and asserts that a guest stack trace is
//! printed, optionally blaming a specific source line.
//!
//! The search bisects over chaos schedules and relies on PMU branch counters
//! plus working user/mount namespaces, so the tests are `#[ignore]`d by default
//! (like the `chaos_buck_*` cases in `hermit_modes.rs`) and are exercised
//! explicitly by `scripts/validate.rs`. Run them with:
//!
//! ```text
//! cargo test -p hermit-run --test analyze -- --ignored
//! ```

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

static ANALYZE_LOCK: Mutex<()> = Mutex::new(());
static WORKLOADS: OnceLock<AnalyzeWorkloads> = OnceLock::new();
const ANALYZE_SKID_MARGIN_ENV: &str = "HERMIT_ANALYZE_SKID_MARGIN";

/// Guest binaries whose races `hermit analyze` is expected to pinpoint.
///
/// They are compiled beneath `CARGO_TARGET_TMPDIR` (inside the repository's
/// `target/` directory) rather than `/tmp`, because `hermit analyze` bind-mounts
/// its own workspace over `/tmp` in the guest and would otherwise shadow the
/// binary.
struct AnalyzeWorkloads {
    hello_race: PathBuf,
    racewrite_nostdlib: PathBuf,
    nanosleep_nocrash: PathBuf,
    fd_reuse_close_race: PathBuf,
    race_then: PathBuf,
    refusal_after_race: PathBuf,
    rpc_shutdown_removes_client: PathBuf,
}

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn analyze_lock() -> MutexGuard<'static, ()> {
    ANALYZE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn add_calibrated_skid_margin(command: &mut Command, label: &str) {
    let Some(value) = std::env::var_os(ANALYZE_SKID_MARGIN_ENV) else {
        return;
    };
    let value = value.to_string_lossy();
    let margin = value.parse::<u64>().unwrap_or_else(|error| {
        panic!(
            "{label}: {ANALYZE_SKID_MARGIN_ENV} must be a positive integer, got {value:?}: {error}"
        )
    });
    assert!(
        margin > 0,
        "{label}: {ANALYZE_SKID_MARGIN_ENV} must be greater than zero"
    );
    eprintln!("{label}: using calibrated PMU skid margin {margin} RCB");
    command.arg(format!("--run-arg=--skid-margin={margin}"));
}

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

/// The `--tmp-dir` a test's analysis creates its workspace in, beside its
/// report. Analyze removes the workspace when it exits, unless `--keep-tmp`.
fn tmp_dir_in(report_dir: &Path) -> PathBuf {
    report_dir.join("workspaces")
}

/// What an analysis left in its `--tmp-dir` (or any directory).
fn left_in(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("cannot list {}: {error}", dir.display()),
    }
}

/// An analysis that has exited left no workspace in `tmp_dir`: one that
/// finished, or one a stop signal ended.
fn assert_no_workspace_left(label: &str, tmp_dir: &Path, combined: &str) {
    let left = left_in(tmp_dir);
    assert!(
        left.is_empty(),
        "{label}: analyze left {left:?} in its --tmp-dir\n{combined}"
    );
}

/// An analysis that ended with an error kept its one workspace in `tmp_dir`,
/// said why on the line that names it, and the workspace still holds
/// `evidence`, files of the trials the error is about.
fn assert_workspace_kept_after_error(
    label: &str,
    tmp_dir: &Path,
    combined: &str,
    evidence: &[&str],
) {
    let left = left_in(tmp_dir);
    assert!(
        left.len() == 1 && evidence.iter().all(|file| left[0].join(file).is_file()),
        "{label}: analyze kept {left:?} in its --tmp-dir, not one workspace holding \
         {evidence:?}\n{combined}"
    );
    assert!(
        combined.contains(&format!(
            ":: Kept the workspace at {} (the error above can name files in it)",
            left[0].display()
        )),
        "{label}: analyze did not say it kept the workspace for the error\n{combined}"
    );
}

fn compile_c_pthread(source: &Path, output: &Path) {
    let mut command = Command::new("cc");
    command
        .args(["-O0", "-g", "-pthread", "-D_GNU_SOURCE"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "C workload compilation");
}

fn compile_c_without_libc(source: &Path, output: &Path) {
    let mut command = Command::new("cc");
    command
        .args(["-g", "-nostdlib"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "C workload compilation without libc");
}

fn compile_rust(source: &Path, output: &Path) {
    let mut command = Command::new("rustc");
    command
        .args(["--edition=2024", "-C", "debuginfo=1"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "Rust workload compilation");
}

fn workloads() -> &'static AnalyzeWorkloads {
    WORKLOADS.get_or_init(|| {
        let repository = repository();
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hermit-analyze-workloads");
        fs::create_dir_all(&build_root).expect("failed to create analyze workload directory");

        let hello_race = build_root.join("hello_race");
        compile_rust(&repository.join("flaky-tests/hello_race.rs"), &hello_race);

        let racewrite_nostdlib = build_root.join("racewrite_nostdlib");
        compile_c_without_libc(
            &repository.join("tests/c/simple/racewrite_nostdlib.c"),
            &racewrite_nostdlib,
        );

        let nanosleep_nocrash = build_root.join("nanosleep_nocrash");
        compile_c_pthread(
            &repository.join("tests/c/simple/nanosleep-threads-nocrash.c"),
            &nanosleep_nocrash,
        );

        let fd_reuse_close_race = build_root.join("fd_reuse_close_race");
        compile_c_pthread(
            &repository.join("tests/c/fd_reuse_close_race.c"),
            &fd_reuse_close_race,
        );

        let race_then = build_root.join("analyze_race_then");
        compile_c_pthread(
            &repository.join("hermit-cli/tests/fixtures/analyze_race_then.c"),
            &race_then,
        );

        let refusal_after_race = build_root.join("analyze_refusal_after_race");
        compile_c_pthread(
            &repository.join("hermit-cli/tests/fixtures/analyze_refusal_after_race.c"),
            &refusal_after_race,
        );

        let rpc_shutdown_removes_client = build_root.join("rpc_shutdown_removes_client");
        compile_c_pthread(
            &repository.join("tests/c/rpc_shutdown_removes_client.c"),
            &rpc_shutdown_removes_client,
        );

        AnalyzeWorkloads {
            hello_race,
            racewrite_nostdlib,
            nanosleep_nocrash,
            fd_reuse_close_race,
            race_then,
            refusal_after_race,
            rpc_shutdown_removes_client,
        }
    })
}

/// Runs `hermit analyze --search` over `guest` and returns its report file,
/// asserting a successful exit and that at least one guest stack trace was
/// printed. `expected_output`, when non-empty, must also appear in the
/// combined stdout+stderr (used to pin the blamed source line).
///
/// Mirrors `tests/util/hermit_analyze_test.sh`: `--analyze-seed=0 --search`,
/// then a chaotic run with a tight preemption timeout so the search has enough
/// scheduling change points to expose the race.
fn run_analyze(
    label: &str,
    guest: &Path,
    analyze_opts: &[&str],
    expected_output: &str,
) -> serde_json::Value {
    let _guard = analyze_lock();
    let report_dir = tempfile::tempdir().expect("failed to create analyze report directory");
    let report_file = report_dir.path().join("report.json");
    let tmp_dir = tmp_dir_in(report_dir.path());

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.arg("analyze");
    command.args(analyze_opts);
    add_calibrated_skid_margin(&mut command, label);
    command
        .arg(format!("--report-file={}", report_file.display()))
        .arg(format!("--tmp-dir={}", tmp_dir.display()))
        .args(["--analyze-seed=0", "--search", "--"])
        // Arguments forwarded to the underlying `hermit run` invocations:
        .args(["--chaos", "--summary", "--max-timeslice=400000", "--"])
        .arg(guest);

    let output = command_output(command, label);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    assert!(
        combined.contains("Stack trace for thread"),
        "{label}: hermit analyze did not print a guest stack trace\n{combined}"
    );
    // The report is the user's; the workspace is analyze's.
    assert!(report_file.is_file(), "{label}: no report\n{combined}");
    assert_no_workspace_left(label, &tmp_dir, &combined);
    // Automatic bounds grow with completed endpoint measurements. The exact
    // formula is unit-tested; a slow host may legitimately announce more than 60.
    let bounds: Vec<u64> = combined
        .lines()
        .filter_map(|line| line.split_once("Runs from here on are bounded at "))
        .map(|(_, rest)| {
            rest.split_once(" s of wall-clock time")
                .expect("the announcement must name seconds")
                .0
                .parse()
                .expect("the announced bound must be an integer")
        })
        .collect();
    assert!(
        !bounds.is_empty(),
        "{label}: no automatic trial bound\n{combined}"
    );
    assert!(
        bounds.iter().all(|bound| *bound >= 60),
        "{label}: {bounds:?}"
    );
    assert!(
        bounds.windows(2).all(|pair| pair[0] < pair[1]),
        "{label}: {bounds:?}"
    );
    if !expected_output.is_empty() {
        assert!(
            combined.contains(expected_output),
            "{label}: expected {expected_output:?} in hermit analyze output\n{combined}"
        );
    }

    // Analyze writes the branch-counter warning to stderr at most once, before
    // its first trial, and the report repeats exactly that line, without its
    // newline; it carries none where analyze wrote none.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let warnings: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("WARNING: this host's retired-branch counter"))
        .collect();
    assert!(warnings.len() <= 1, "{label}: {warnings:?}\n{combined}");
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report_file).expect("hermit analyze should write its report file"),
    )
    .expect("the analyze report is JSON");
    assert_eq!(
        report
            .get("branch_counter_warning")
            .map(|value| value.as_str().expect("branch_counter_warning is a string")),
        warnings.first().copied(),
        "{label}: {report}\n{combined}"
    );
    if let Some(warning) = warnings.first() {
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!(
                "------------------------------ hermit analyze report ------------------------------\n{warning}\n"
            )),
            "{label}: the report banner must be followed by its branch-counter caveat\n{combined}"
        );
    }
    let report = fs::read_to_string(&report_file)
        .unwrap_or_else(|error| panic!("{label}: no report file: {error}\n{combined}"));
    serde_json::from_str(&report)
        .unwrap_or_else(|error| panic!("{label}: unreadable report file: {error}\n{report}"))
}

/// Exercises analyze's fail-closed endpoint check for a workload whose target
/// outcome now also occurs under the non-chaos baseline schedule.
fn run_analyze_expect_baseline_collision(label: &str, guest: &Path, analyze_opts: &[&str]) {
    let _guard = analyze_lock();
    let report_dir = tempfile::tempdir().expect("failed to create analyze report directory");
    let report_file = report_dir.path().join("report.json");
    let tmp_dir = tmp_dir_in(report_dir.path());

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.arg("analyze");
    command.args(analyze_opts);
    add_calibrated_skid_margin(&mut command, label);
    command
        .arg(format!("--report-file={}", report_file.display()))
        .arg(format!("--tmp-dir={}", tmp_dir.display()))
        .args(["--analyze-seed=0", "--search", "--"])
        .args(["--chaos", "--summary", "--preemption-timeout=400000", "--"])
        .arg(guest);

    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    assert!(
        !output.status.success(),
        "{label} unexpectedly found distinct target and baseline schedules\n{combined}"
    );
    assert!(
        combined.contains("baseline run matched target criteria when it should not"),
        "{label}: analyze did not reject indistinguishable endpoints\n{combined}"
    );
    // The error is about the baseline run: its reproducer and the output that
    // met the target stay to be read.
    assert_workspace_kept_after_error(
        label,
        &tmp_dir,
        &combined,
        &["run2_baseline.repro", "run2_baseline.stdout"],
    );
}

#[test]
#[ignore = "slow: bisecting chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_hello_race() {
    run_analyze(
        "analyze hello_race",
        &workloads().hello_race,
        &["--run-arg=--base-env=host"],
        "flaky-tests/hello_race.rs:37",
    );
}

#[test]
#[ignore = "slow: bisecting chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_racewrite_nostdlib() {
    run_analyze(
        "analyze racewrite_nostdlib",
        &workloads().racewrite_nostdlib,
        &[
            "--selfcheck",
            "--run-arg=--base-env=empty",
            "--target-exit-code=0",
            // The current non-chaos baseline is barfoo; analyze the opposite
            // write order so the endpoints remain behaviorally distinct.
            "--target-stdout=foobar",
        ],
        // The racing `write` syscall lives at this line in the guest source.
        "racewrite_nostdlib.c:35",
    );
}

/// `--prefix-search` finds the same race from the target schedule alone: the
/// deciding event is the parent's return from fork, and the thread the
/// scheduler would run there instead is the child at its racing write. The
/// parent's slice ends right after that return in the target schedule, and a
/// trial that stops there leaves that slice end to the scheduler; one that
/// replayed it would name the parent's next event, one too late.
#[test]
#[ignore = "slow: bisecting chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_racewrite_nostdlib_prefix_search() {
    let report = run_analyze(
        "analyze racewrite_nostdlib --prefix-search",
        &workloads().racewrite_nostdlib,
        &[
            "--prefix-search",
            "--run-arg=--base-env=empty",
            "--target-exit-code=0",
            "--target-stdout=foobar",
        ],
        "racewrite_nostdlib.c:35",
    );
    let header = report["header"].as_str().unwrap_or_default();
    assert!(header.contains("Syscall(fork, Posthook)"), "{report}");
    let thread = |event: &str| report[event]["stack"]["thread_id"].as_i64();
    assert!(thread("critical_event1").is_some(), "{report}");
    assert!(thread("critical_event2").is_some(), "{report}");
    assert_ne!(
        thread("critical_event1"),
        thread("critical_event2"),
        "{report}"
    );
}

/// A shutdown that removes a client before the server looks it up
/// (tests/c/rpc_shutdown_removes_client.c): the race is in memory, so the
/// finder can only rank candidates, first the server's change of course
/// against the deciding event. The report checks each candidate by
/// intervention. Moving the server's turn before the shutdown's crosses the
/// client and the poller, and the check says how many events of each; the
/// server then cannot run as edited (it needs the request that now comes
/// after it), so the check is not realized rather than a verdict. No
/// poller nap decides.
#[test]
#[ignore = "slow: replays chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_checks_the_race_candidates_and_says_what_each_edit_moved() {
    let report = run_analyze(
        "analyze rpc_shutdown_removes_client --prefix-search",
        &workloads().rpc_shutdown_removes_client,
        &[
            "--prefix-search",
            "--run-arg=--base-env=empty",
            "--target-stdout=call failed",
        ],
        "call failed",
    );
    let racing = &report["racing_accesses"];
    assert_eq!(racing["status"], "candidates", "{report}");
    let candidates = racing["candidates"].as_array().cloned().unwrap_or_default();
    let calls = |c: &serde_json::Value| -> Vec<String> {
        (0..2)
            .map(|i| c["accesses"][i]["call"].as_str().unwrap_or("").to_owned())
            .collect()
    };
    let change = candidates
        .iter()
        .find(|c| calls(c).iter().any(|call| call.contains("not found")))
        .unwrap_or_else(|| panic!("no candidate names the server's change: {report}"));
    let check = &change["check"];
    assert_eq!(check["verdict"], "not_realized", "{report}");
    assert!(
        check["edit"].as_str().unwrap_or("").starts_with("moving ")
            && check["moved"]["events"].as_u64().unwrap_or(0) > 0
            && !check["moved"]["crossed"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .is_empty(),
        "{report}"
    );
    for candidate in &candidates {
        assert!(candidate["check"].is_object(), "{report}");
        if calls(candidate)
            .iter()
            .any(|call| call.starts_with("nanosleep("))
        {
            assert_ne!(candidate["check"]["verdict"], "decides", "{report}");
        }
    }
}

/// A deferred second close of a descriptor number another thread reopened
/// (tests/c/fd_reuse_close_race.c, the fdsan pattern): the report's
/// "Racing accesses" section names the closer's close and the writer's lseek
/// that fails with EBADF, on two threads, and no edge Hermit observes orders
/// them.
#[test]
#[ignore = "slow: bisecting chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_names_the_close_racing_the_reused_descriptor() {
    let report = run_analyze(
        "analyze fd_reuse_close_race --prefix-search",
        &workloads().fd_reuse_close_race,
        &[
            "--prefix-search",
            "--run-arg=--base-env=empty",
            "--target-stderr=failed with EBADF",
        ],
        "unordered by the edges Hermit observes",
    );
    let racing = &report["racing_accesses"];
    assert_eq!(racing["status"], "found", "{report}");
    let access = |i: usize, field: &str| racing["accesses"][i][field].clone();
    let call = |i: usize| access(i, "call").as_str().unwrap_or_default().to_owned();
    assert!(call(0).starts_with("close("), "{report}");
    assert!(call(1).starts_with("lseek("), "{report}");
    assert_eq!(access(1, "result"), "-EBADF", "{report}");
    assert_ne!(access(0, "thread"), access(1, "thread"), "{report}");
    assert_eq!(
        racing["happens_before"]["target"]["ordered"], false,
        "{report}"
    );
}

#[test]
#[ignore = "slow: bisecting chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_nanosleep_threads_rejects_indistinguishable_baseline() {
    run_analyze_expect_baseline_collision(
        "analyze nanosleep-threads baseline collision",
        &workloads().nanosleep_nocrash,
        &["--run-arg=--base-env=empty", "--target-exit-code=0"],
    );
}

/// A stopped replay establishes neither a baseline outcome nor a realized
/// schedule. The outer bound is distinct from Hermit's own timeout marker.
#[test]
#[ignore = "requires working mount namespaces; selected by the calibrated analyze validation node"]
fn bisect_rejects_a_timed_out_replay() {
    use detcore::preemptions::PreemptionRecord;
    use detcore::types::DetTid;
    use detcore::types::SchedEvent;
    use detcore::types::SyscallPhase;
    use reverie::syscalls::Sysno;

    let _guard = analyze_lock();
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.json");
    let bad = dir.path().join("bad.json");
    let report = dir.path().join("report.json");
    for (path, syscall) in [(&good, Sysno::getpid), (&bad, Sysno::getppid)] {
        let record = PreemptionRecord::from_sched_events(vec![SchedEvent::syscall(
            DetTid::from_raw(3),
            syscall,
            SyscallPhase::Prehook,
        )]);
        record.validate().unwrap();
        record.write_to_disk(path).unwrap();
    }
    let mut command = Command::new("timeout");
    command
        .args(["--signal=KILL", "30s"])
        .arg(hermit_test::hermit_binary())
        .arg("bisect")
        .arg(format!("--good={}", good.display()))
        .arg(format!("--bad={}", bad.display()))
        .arg(format!("--report-file={}", report.display()))
        .args([
            "--",
            "--timeout=1",
            "--no-rcb-time",
            "--max-timeslice=disabled",
            "--",
            "/bin/sh",
            "-c",
            "while :; do :; done",
        ]);
    let output = hermit_test::hermit_output("bisect timed-out replay", &mut command);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(124), "{combined}");
    assert!(
        combined.contains("HERMIT_RUN_TIMEOUT class=run-timeout"),
        "{combined}"
    );
    assert!(combined.contains("larger --timeout"), "{combined}");
    assert!(
        !report.exists(),
        "an unfinished replay wrote a report: {combined}"
    );
    for unsupported_claim in [
        "counted as the baseline condition",
        "Completed analysis successfully",
        "reproduces the good outcome",
        "determines the program outcome",
        "decides the outcome",
        "hermit analyze report",
    ] {
        assert!(
            !combined.contains(unsupported_claim),
            "{unsupported_claim}: {combined}"
        );
    }
}

/// Find a b-first seed using the fixture's terminating mode. The race is the
/// same with `spin`; only the behavior after a wins changes. This keeps seed
/// discovery bounded without waiting for every losing seed to time out.
fn race_then_target_seed() -> u64 {
    race_then_target_seed_matching("exit", &[])
}

/// [`race_then_target_seed`] in mode `stand_in`, under the run arguments
/// `run_args`. The fixture acts on its mode only after the race, so a
/// `stand_in` that terminates whichever thread wins finds the seed for any
/// mode with the same argc and argument length before the race.
fn race_then_target_seed_matching(stand_in: &str, run_args: &[&str]) -> u64 {
    let guest = &workloads().race_then;
    for seed in 0..40 {
        let mut command = Command::new("timeout");
        command
            .args(["--signal=KILL", "30s"])
            .arg(hermit_test::hermit_binary())
            .args(["run", "--chaos", "--timeout=2"])
            .args(run_args)
            .arg(format!("--seed={seed}"));
        if let Some(margin) = std::env::var_os(ANALYZE_SKID_MARGIN_ENV) {
            command.arg(format!("--skid-margin={}", margin.to_string_lossy()));
        }
        // Match the mode's argc and argument length before the race. The mode
        // is inspected only after the winning thread is known.
        command.arg("--").arg(guest).arg(stand_in);
        let output = hermit_test::hermit_output("terminating race seed discovery", &mut command);
        match output.status.code() {
            Some(0) => {
                assert_eq!(output.stdout, b"b-first\n");
                return seed;
            }
            // EXIT-CLASS: guest. The spin guest's exit status is forwarded.
            Some(1) => assert_eq!(output.stdout, b"a-first\n"),
            _ => panic!("seed discovery did not complete: {output:?}"),
        }
    }
    panic!("no chaos seed in 0..40 lets thread b win the race");
}

fn assert_spin_trial_stops_analysis(analyze_opts: &[&str], expected_run: &str) {
    let _guard = analyze_lock();
    let seed = race_then_target_seed();
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    let mut command = Command::new("timeout");
    command
        .args(["--signal=KILL", "30s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "analyze",
            "--target-stdout=b-first",
            "--target-exit-code=0",
            "--trial-timeout=3",
        ])
        .args(analyze_opts)
        .arg(format!("--run1-seed={seed}"))
        .arg(format!("--report-file={}", report.display()));
    add_calibrated_skid_margin(&mut command, expected_run);
    command
        .args(["--", "--chaos", "--"])
        .arg(&workloads().race_then)
        .arg("spin");
    let output = hermit_test::hermit_output("analyze bounded spin trial", &mut command);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(output.status.code(), Some(124), "{combined}");
    assert!(
        combined.contains("HERMIT_RUN_TIMEOUT class=run-timeout"),
        "{combined}"
    );
    assert!(
        combined.contains("First run matched target criteria"),
        "{combined}"
    );
    assert!(
        combined.contains(&format!(
            "{expected_run} reached hermit analyze's bound of 3 s per run"
        )),
        "the intended later run must hit the bound: {combined}"
    );
    assert!(
        combined.contains("no program outcome was established"),
        "{combined}"
    );
    assert!(
        !report.exists(),
        "an unfinished trial wrote a report: {combined}"
    );
    for unsupported_claim in [
        "counted as the baseline condition",
        "Completed analysis successfully",
        "reproduces the good outcome",
        "determines the program outcome",
        "decides the outcome",
        "hermit analyze report",
        "Searching within the target schedule instead",
    ] {
        assert!(
            !combined.contains(unsupported_claim),
            "{unsupported_claim}: {combined}"
        );
    }
}

/// A prefix continued without chaos spins forever. Its timeout must stop the
/// search rather than become a completed non-target outcome.
#[test]
#[ignore = "requires PMU and mount namespaces; selected by the calibrated analyze validation node"]
fn analyze_bounds_trials_that_never_end() {
    assert_spin_trial_stops_analysis(&["--prefix-search"], "prefix_round_001");
}

/// A timed-out default baseline cannot become an endpoint or justify a report.
#[test]
#[ignore = "requires PMU and mount namespaces; selected by the calibrated analyze validation node"]
fn analyze_rejects_a_baseline_that_never_ends() {
    assert_spin_trial_stops_analysis(&[], "run2_baseline");
}
/// (https://github.com/rrnewton/hermit/issues/4812). The guest is named by a
/// path relative to analyze's working directory, so a reproducer that does
/// not start in that directory cannot even find its program. The test reads
/// the baseline run's reproducer, which analyze writes before its search, so
/// it does not depend on how the search ends.
#[test]
#[ignore = "slow: bisecting chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_reproducer_runs_from_another_directory() {
    let label = "analyze reproducer from another directory";
    let _guard = analyze_lock();
    let guest = &workloads().hello_race;
    let guest_dir = guest.parent().unwrap();
    let guest_name = Path::new(".").join(guest.file_name().unwrap());

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.current_dir(guest_dir).arg("analyze");
    add_calibrated_skid_margin(&mut command, label);
    command
        .args([
            "--analyze-seed=0",
            "--search",
            "--run-arg=--base-env=host",
            "--",
        ])
        .args(["--chaos", "--summary", "--max-timeslice=400000", "--"])
        .arg(&guest_name);
    let output = command.output().expect("failed to run hermit analyze");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let workspace = stderr
        .lines()
        .find_map(|line| line.strip_prefix(":: Temp workspace: "))
        .unwrap_or_else(|| panic!("{label}: no workspace in analyze's output\n{stderr}"));
    let repro = Path::new(workspace).join("run2_baseline.repro");
    let line = fs::read_to_string(&repro)
        .unwrap_or_else(|error| panic!("{label}: {}: {error}\n{stderr}", repro.display()));

    // The line names `hermit` as a command; run it with this build, from the
    // guest's directory and from another one, and compare the two runs.
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_hermit")).parent().unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let elsewhere = tempfile::tempdir().unwrap();
    let run_from = |dir: &Path| {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(line.trim())
            .current_dir(dir)
            .env("PATH", &path);
        command.output().unwrap()
    };
    let here = run_from(guest_dir);
    let there = run_from(elsewhere.path());
    assert!(
        here.status.success(),
        "{label}: {} failed from the guest's directory\n{}",
        repro.display(),
        String::from_utf8_lossy(&here.stderr),
    );
    assert_eq!(
        (there.status.code(), &there.stdout),
        (here.status.code(), &here.stdout),
        "{label}: {} reproduced its run from the guest's directory but not from {}\n{}",
        repro.display(),
        elsewhere.path().display(),
        String::from_utf8_lossy(&there.stderr),
    );
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_no_refusal_report(output: &Output, report: &Path) -> String {
    let combined = combined_output(output);
    // EXIT-CLASS: hermit (the policy refusal's own exit status)
    assert_eq!(output.status.code(), Some(122), "{combined}");
    assert!(
        combined.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
        "{combined}"
    );
    assert!(
        combined.contains("no program outcome was established"),
        "{combined}"
    );
    assert!(
        combined.contains("use a supported operation or configuration before retrying"),
        "{combined}"
    );
    assert!(
        !report.exists(),
        "a refused trial wrote a report: {combined}"
    );
    for claim in [
        "counted as the baseline condition",
        "counted as not reaching",
        "Completed analysis successfully",
        "Critical events found which exercise race bug",
        "reproduces the good outcome",
        "determines the program outcome",
        "decides the outcome",
        "hermit analyze report",
    ] {
        assert!(!combined.contains(claim), "{claim}: {combined}");
    }
    combined
}

/// Discovery uses a mode in which either race order completes; the actual
/// refused-mode target must independently complete under analyze before its
/// later trial may be tested.
fn completed_refusal_fixture_seed(guest: &Path) -> u64 {
    for seed in 0..40 {
        let mut command = Command::new("timeout");
        command
            .args(["--signal=KILL", "30s"])
            .arg(hermit_test::hermit_binary())
            .args(["run", "--chaos", "--timeout=3"])
            .arg(format!("--seed={seed}"));
        if let Some(margin) = std::env::var_os(ANALYZE_SKID_MARGIN_ENV) {
            command.arg(format!("--skid-margin={}", margin.to_string_lossy()));
        }
        command.args(["--"]).arg(guest).arg("finish");
        let output =
            hermit_test::hermit_output("find completed refusal-fixture seed", &mut command);
        let combined = combined_output(&output);
        assert!(!combined.contains("HERMIT_POLICY_REFUSAL"), "{combined}");
        match output.status.code() {
            Some(0) => {
                assert!(combined.contains("b-first"), "{combined}");
                return seed;
            }
            // EXIT-CLASS: guest. The a-first fixture guest exits 122 itself;
            // the assertion above refuses a hermit policy refusal here.
            Some(122) => assert!(combined.contains("a-first"), "{combined}"),
            status => panic!("seed {seed}: unexpected setup status {status:?}\n{combined}"),
        }
    }
    panic!("no completed b-first target found in 40 bounded fixture runs");
}

/// A completed target does not turn a refused prefix trial into a baseline.
#[test]
#[ignore = "requires PMU and mount namespaces; selected by the calibrated analyze validation node"]
fn analyze_stops_on_a_refused_trial() {
    let _guard = analyze_lock();
    let guest = &workloads().refusal_after_race;
    let seed = completed_refusal_fixture_seed(guest);
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    let mut command = Command::new("timeout");
    command
        .args(["--signal=KILL", "30s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "analyze",
            "--prefix-search",
            "--target-stdout=b-first",
            "--target-exit-code=0",
        ])
        .arg(format!("--run1-seed={seed}"))
        .arg(format!("--report-file={}", report.display()));
    add_calibrated_skid_margin(&mut command, "analyze refused prefix trial");
    command
        .args(["--", "--chaos", "--timeout=3", "--"])
        .arg(guest)
        .arg("refuse");
    let output = hermit_test::hermit_output("analyze refused prefix trial", &mut command);
    let combined = assert_no_refusal_report(&output, &report);
    assert!(
        combined.contains("First run matched target criteria"),
        "{combined}"
    );
    assert!(
        combined.contains("Hermit refused analysis run prefix_round_001"),
        "{combined}"
    );
    assert!(
        combined.contains("unsupported syscall: restart_syscall"),
        "{combined}"
    );
    assert!(combined.contains("Hermit's stderr above"), "{combined}");
}

/// A refused target has no outcome, while a guest's ordinary exit 122 does.
/// With the trials logged into files (`--trial-log=debug`) the error must
/// name the actual retained trial log; with `--verbose` alone, which no
/// longer sets the trials' level, it points at Hermit's stderr, where the
/// refusal's reason is.
#[test]
#[ignore = "requires PMU and mount namespaces; selected by the calibrated analyze validation node"]
fn analyze_ends_with_a_refused_target_run() {
    let _guard = analyze_lock();
    let mut ordinary = Command::new("timeout");
    ordinary
        .args(["--signal=KILL", "30s"])
        .arg(hermit_test::hermit_binary())
        .args(["run", "--timeout=3", "--", "/bin/sh", "-c", "exit 122"]);
    let ordinary = hermit_test::hermit_output("ordinary guest exit 122", &mut ordinary);
    // EXIT-CLASS: guest. The guest's own `exit 122`, forwarded by hermit.
    assert_eq!(ordinary.status.code(), Some(122));
    assert!(!combined_output(&ordinary).contains("HERMIT_POLICY_REFUSAL"));

    let refused = |trial_log: &[&str]| {
        let dir = tempfile::tempdir().unwrap();
        let workspace_parent = dir.path().join("workspace");
        let report = dir.path().join("report.json");
        let mut command = Command::new("timeout");
        command
            .args(["--signal=KILL", "30s"])
            .arg(hermit_test::hermit_binary())
            .args(["analyze", "--verbose"])
            .args(trial_log)
            .args(["--prefix-search", "--target-stdout=a-first"])
            .arg(format!("--tmp-dir={}", workspace_parent.display()))
            .arg(format!("--report-file={}", report.display()));
        add_calibrated_skid_margin(&mut command, "analyze refused target");
        command
            .args(["--", "--timeout=3", "--"])
            .arg(&workloads().refusal_after_race)
            .arg("refuse");
        let output = hermit_test::hermit_output("analyze refused target", &mut command);
        let combined = assert_no_refusal_report(&output, &report);
        (dir, workspace_parent, output, combined)
    };
    // --verbose alone: the refusal's reason is on Hermit's stderr.
    let (_dir, _, output, combined) = refused(&[]);
    assert!(
        combined.contains("Hermit refused analysis run run1_target")
            && combined.contains("Hermit's stderr above"),
        "{combined}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unsupported syscall: restart_syscall"),
        "{combined}"
    );
    let (_dir, workspace_parent, _, combined) = refused(&["--trial-log=debug"]);
    assert!(
        combined.contains("Hermit refused analysis run run1_target"),
        "{combined}"
    );
    assert!(
        !combined.contains("First run matched target criteria"),
        "{combined}"
    );
    let workspaces: Vec<_> = fs::read_dir(&workspace_parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(workspaces.len(), 1);
    let log = workspaces[0].join("run1_target.log");
    assert!(
        combined.contains(&format!("the trial log {}", log.display())),
        "{combined}"
    );
    let diagnostics = fs::read_to_string(log).unwrap();
    assert!(
        diagnostics.contains("unsupported syscall: restart_syscall"),
        "{diagnostics}"
    );
}

/// `hermit analyze` of the analyze_race_then fixture in mode `then` (one
/// that terminates whichever thread wins), with `analyze_opts` and the run
/// arguments `run_args`, from a chaos seed under which thread b wins: the
/// output, and the report if analyze wrote one. It runs under the analyze
/// lock and an outer bound, with the calibrated skid margin, as the spin
/// tests do.
fn analyze_race_then_with(
    then: &str,
    analyze_opts: &[&str],
    run_args: &[&str],
) -> (Output, Option<serde_json::Value>) {
    let _guard = analyze_lock();
    let seed = race_then_target_seed_matching(then, run_args);
    let dir = tempfile::tempdir().unwrap();
    let report_file = dir.path().join("report.json");
    let tmp_dir = tmp_dir_in(dir.path());
    let mut command = Command::new("timeout");
    command
        .args(["--signal=KILL", "120s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "analyze",
            "--target-stdout=b-first",
            "--target-exit-code=0",
            "--trial-timeout=3",
        ])
        .args(analyze_opts)
        .arg(format!("--run1-seed={seed}"))
        .arg(format!("--report-file={}", report_file.display()))
        .arg(format!("--tmp-dir={}", tmp_dir.display()));
    add_calibrated_skid_margin(&mut command, then);
    command
        .args(["--", "--chaos"])
        .args(run_args)
        .arg("--")
        .arg(&workloads().race_then)
        .arg(then);
    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    if output.status.success() {
        assert_no_workspace_left(&format!("{command:?}"), &tmp_dir, &combined_output(&output));
    } else {
        assert_workspace_kept_after_error(
            &format!("{command:?}"),
            &tmp_dir,
            &combined_output(&output),
            &["run1_target.repro"],
        );
    }
    let report = fs::read_to_string(&report_file).ok().map(|text| {
        serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!("unreadable report: {error}\n{}", combined_output(&output))
        })
    });
    (output, report)
}

/// A race found only at a short timeslice: at the run's own 100 us, thread a
/// is preempted before it claims even without chaos, so the scheduler alone
/// meets the target and gives the prefix search nothing to bisect against.
/// The search says so and continues at hermit's default 200 ms, where it
/// does not. Before, the analysis ended there with exit 125.
#[test]
#[ignore = "slow: replays chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_continues_prefixes_at_a_coarser_timeslice() {
    let label = "analyze --prefix-search analyze_race_then late-a --max-timeslice=100000";
    let (output, report) =
        analyze_race_then_with("late-a", &["--prefix-search"], &["--max-timeslice=100000"]);
    let combined = combined_output(&output);
    assert!(
        output.status.success(),
        "{label}: {}\n{combined}",
        output.status
    );
    assert!(
        combined.contains(
            "Continuing without chaos at a 100000 ns timeslice, the scheduler alone, replaying \
             none of the target schedule, meets the target"
        ),
        "{label}\n{combined}"
    );
    let report = report.unwrap_or_else(|| panic!("{label}: no report\n{combined}"));
    assert!(
        report["header"]
            .as_str()
            .unwrap()
            .contains("scheduling without chaos at a 200000000 ns timeslice does not meet"),
        "{label}: {report}"
    );
}

/// The same race with `--continuation-timeslice=100000`: there is no other
/// continuation to try, so analyze fails, saying what its empty-prefix trial
/// observed (no miss to bisect against) and what to run or pass instead.
#[test]
#[ignore = "slow: replays chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_explains_a_target_the_scheduler_meets_alone() {
    let label = "analyze --prefix-search analyze_race_then late-a --continuation-timeslice=100000";
    let (output, _) = analyze_race_then_with(
        "late-a",
        &["--prefix-search", "--continuation-timeslice=100000"],
        &["--max-timeslice=100000"],
    );
    let combined = combined_output(&output);
    assert!(!output.status.success(), "{label}\n{combined}");
    for expected in [
        "no continuation gives the prefix search a miss to bisect against: replaying none of the \
         target schedule and then scheduling without chaos, the scheduler alone already meets \
         the target criteria at a 100000 ns timeslice",
        "a longer prefix may still miss the target",
        "prefix_round_001.repro",
        "pass --continuation-timeslice",
        "--run2-seed",
    ] {
        assert!(
            combined.contains(expected),
            "{label}: no {expected:?}\n{combined}"
        );
    }
}

/// An analysis that ends with an error keeps its workspace, with or without
/// `--keep-tmp`, since the error can name trial files in it (main's
/// `incomplete_trial` says they are retained): the target run here misses a
/// target nothing prints, and without `--search` analyze ends there.
#[test]
#[ignore = "slow: runs a chaos trial; requires PMU branch counters and working mount namespaces"]
fn analyze_keeps_its_workspace_after_an_error() {
    for keep in [false, true] {
        let label = format!("analyze hello_race, target never met, keep: {keep}");
        let _guard = analyze_lock();
        let dir = tempfile::tempdir().expect("failed to create analyze directory");
        let tmp_dir = tmp_dir_in(dir.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command
            .arg("analyze")
            .arg(format!("--tmp-dir={}", tmp_dir.display()))
            .args(["--run-arg=--base-env=host", "--target-stdout=never printed"]);
        if keep {
            command.arg("--keep-tmp");
        }
        command
            .args(["--", "--chaos", "--max-timeslice=400000", "--"])
            .arg(&workloads().hello_race);
        let output = hermit_test::hermit_output(&label, &mut command);
        let combined = combined_output(&output);
        assert!(
            !output.status.success()
                && combined.contains("The run did not match the target criteria"),
            "{label}: {}\n{combined}",
            output.status
        );
        let left = left_in(&tmp_dir);
        assert!(
            left.len() == 1 && left[0].join("run1_target.repro").is_file(),
            "{label}: {left:?}\n{combined}"
        );
        assert!(
            combined.contains(if keep {
                "Kept the workspace at"
            } else {
                "(the error above can name files in it)"
            }),
            "{label}\n{combined}"
        );
    }
}

/// Makes a container init wait before it arms its own stop handlers, under
/// the thread name [`ARMING_STALL_COMM`]; both are copied from
/// `stall_container_init_before_arming_if_asked` in `bin/hermit/container.rs`.
const ARMING_STALL_ENV: &str = "HERMIT_INTERNAL_STALL_CONTAINER_INIT_BEFORE_ARMING";
const ARMING_STALL_COMM: &str = "unarmed-init";

/// The children of process `parent`, by the parent field of each
/// `/proc/<pid>/stat`.
fn children_of(parent: u32) -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse::<u32>().ok())
                    == Some(parent)
            })
        })
        .collect()
}

/// Run `hermit analyze` over a target run that spins for ever, with `env`,
/// send SIGTERM to analyze alone once `ready` says so, and check that it ends
/// that run and the analysis: analyze removes its workspace and the run's own
/// temporary directories (its private /tmp, in TMPDIR), leaves no process,
/// and dies of the signal, as a shell reports with 143.
fn analyze_stopped_by_sigterm(label: &str, env: &[(&str, &str)], ready: impl Fn(u32) -> bool) {
    let _guard = analyze_lock();
    let dir = tempfile::tempdir().expect("failed to create analyze directory");
    let tmp_dir = tmp_dir_in(dir.path());
    let run_tmp = dir.path().join("TMPDIR");
    fs::create_dir(&run_tmp).unwrap();
    let stderr = dir.path().join("stderr");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    hermit_test::pin_canonical_epoch(&mut command);
    command
        .arg("analyze")
        .arg(format!("--tmp-dir={}", tmp_dir.display()))
        .args(["--", "--chaos", "--max-timeslice=400000", "--"])
        .args(["/bin/sh", "-c", "while :; do :; done"])
        .env("TMPDIR", &run_tmp)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&stderr).unwrap());
    let mut analyze = command.spawn().expect("failed to start hermit analyze");
    let said = || fs::read_to_string(&stderr).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(120);
    while !ready(analyze.id()) {
        assert!(
            analyze.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "{label}: analyze did not get as far as the signal\n{}",
            said()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let inits = children_of(analyze.id());
    // SAFETY: kill(2) on this test's own child.
    assert_eq!(
        unsafe { libc::kill(analyze.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = analyze.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = analyze.kill();
            let _ = analyze.wait();
            panic!("{label}: analyze did not stop\n{}", said());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let said = said();
    assert_eq!(
        status.signal(),
        Some(libc::SIGTERM),
        "{label}: {status}\n{said}"
    );
    assert!(
        said.contains("hermit analyze stopped on SIGTERM")
            && said.contains("Removed the workspace"),
        "{label}\n{said}"
    );
    assert_no_workspace_left(label, &tmp_dir, &said);
    let run_left = left_in(&run_tmp);
    assert!(
        run_left.is_empty(),
        "{label}: the run left {run_left:?} in TMPDIR\n{said}"
    );
    // Analyze reaped the run's container init before it exited.
    let survivors: Vec<_> = inits
        .iter()
        .filter(|init| Path::new(&format!("/proc/{init}")).exists())
        .collect();
    assert!(
        survivors.is_empty(),
        "{label}: the run's init {survivors:?} outlived analyze\n{said}"
    );
}

/// SIGTERM sent to analyze alone, 2 s into a target run that spins for ever,
/// ends that run and the analysis, and analyze cleans up. Before, the
/// workspace stayed behind whatever ended analyze, and a SIGTERM also left
/// the run's directories.
#[test]
#[ignore = "slow: runs a chaos trial; requires PMU branch counters and working mount namespaces"]
fn analyze_cleans_up_when_a_signal_stops_it() {
    let started = std::cell::Cell::new(None);
    analyze_stopped_by_sigterm("analyze of a guest that spins, SIGTERM", &[], |analyze| {
        // The target run's container init is analyze's only child.
        let at = started
            .get()
            .or((!children_of(analyze).is_empty()).then(Instant::now));
        started.set(at);
        at.is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
    });
}

/// SIGTERM sent to analyze while its run's container init is held before
/// it arms its own stop handlers (Hermit's internal stall) reaches that init
/// all the same: the handler it inherited from analyze ends it as its own
/// would, and analyze cleans up.
#[test]
#[ignore = "slow: runs a chaos trial; requires PMU branch counters and working mount namespaces"]
fn analyze_stops_a_run_whose_init_has_not_armed() {
    analyze_stopped_by_sigterm(
        "analyze, SIGTERM before the run's init arms",
        &[(ARMING_STALL_ENV, "1")],
        |analyze| {
            children_of(analyze).iter().any(|init| {
                fs::read_to_string(format!("/proc/{init}/comm"))
                    .is_ok_and(|comm| comm.trim() == ARMING_STALL_COMM)
            })
        },
    );
}
/// `--target-exit-code` names the exit code as a shell reports it: the
/// guest's own (121 here; with the guest-deadlock outcome, also the code of
/// a run Hermit ends as a deadlock), or 128 plus the signal number when a
/// signal killed the guest (143 for SIGTERM, which dumps no core). Before,
/// analyze compared the raw wait status, so neither matched and analyze
/// said the target run missed the target.
#[test]
#[ignore = "slow: runs hermit analyze; requires PMU branch counters and working mount namespaces"]
fn analyze_targets_the_exit_code_hermit_run_reports() {
    for (code, script) in [
        ("121", "echo TARGET; exit 121"),
        ("143", "echo TARGET; kill -TERM $$"),
    ] {
        let label = format!("analyze --target-exit-code={code} /bin/sh -c {script:?}");
        let _guard = analyze_lock();
        let workspace = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("failed to create a workspace");
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command
            .args([
                "analyze",
                "--prefix-search",
                "--target-stdout=TARGET",
                "--run1-seed=1",
            ])
            .arg(format!("--target-exit-code={code}"))
            .arg(format!("--tmp-dir={}", workspace.path().display()))
            .args(["--", "--", "/bin/sh", "-c", script]);
        let output = hermit_test::hermit_output(&label, &mut command);
        let combined = combined_output(&output);
        assert!(
            combined.contains("First run matched target criteria"),
            "{label}: the target run did not match\n{combined}"
        );
    }
}

/// `--run1-preemptions` replays the record as the target run, measured for
/// the trial bound and logged like a seed's run. Before, analyze did not run
/// it at all, and the next phase unwrapped the log of a run that never ran
/// (exit 101). The record is read from the host's /tmp, which the guest's
/// container does not see, so analyze replays a copy in its workspace. A
/// replay that misses the target ends the analysis with an error that says
/// why.
#[test]
#[ignore = "slow: replays chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_starts_from_a_recorded_target_run() {
    let label = "analyze --prefix-search --run1-preemptions analyze_race_then plain";
    let guest = &workloads().race_then;
    let seed = race_then_target_seed_matching("plain", &[]);
    // Recorded outside /tmp: the run's guest has a private one, and the
    // record is written from inside the container.
    let workspace =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("failed to create a workspace");
    let recorded = workspace.path().join("target.preempts");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args(["run", "--chaos"])
        .arg(format!("--seed={seed}"))
        .arg(format!("--record-preemptions-to={}", recorded.display()))
        .arg("--")
        .arg(guest)
        .arg("plain");
    let output = hermit_test::hermit_output(label, &mut command);
    assert!(
        output.status.success() && output.stdout == b"b-first\n",
        "{label}: the recorded run missed the target\n{}",
        combined_output(&output)
    );
    let host_tmp = tempfile::tempdir_in("/tmp").expect("failed to create a directory under /tmp");
    let record = host_tmp.path().join("target.preempts");
    fs::copy(&recorded, &record).unwrap();

    let _guard = analyze_lock();
    let analyze = |target_stdout: &str, report_file: &Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command
            .args(["analyze", "--prefix-search", "--target-exit-code=0", "-v"])
            .arg(format!("--target-stdout={target_stdout}"))
            .arg(format!("--run1-preemptions={}", record.display()))
            .arg(format!("--report-file={}", report_file.display()))
            .args(["--", "--chaos"])
            .arg(format!("--seed={seed}"))
            .arg("--")
            .arg(guest)
            .arg("plain");
        hermit_test::hermit_output(label, &mut command)
    };
    let report_file = workspace.path().join("report.json");
    let output = analyze("b-first", &report_file);
    let combined = combined_output(&output);
    assert!(
        output.status.success(),
        "{label}: {}\n{combined}",
        output.status
    );
    assert!(
        combined.contains("First run matched target criteria")
            && combined.contains("Runs from here on are bounded at"),
        "{label}: the record was not run as the target\n{combined}"
    );
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&report_file).unwrap()).unwrap();
    assert!(
        report["critical_event1"]["event_index"].is_u64(),
        "{label}: {report}"
    );

    // The record's replay prints b-first, so a-first is a miss.
    let output = analyze("a-first", &workspace.path().join("miss.json"));
    let combined = combined_output(&output);
    assert!(
        !output.status.success()
            && output.status.code() != Some(101)
            && combined.contains("did not meet the target criteria")
            && combined.contains("--seed included"),
        "{label}: a missed replay: {}\n{combined}",
        output.status
    );
}
