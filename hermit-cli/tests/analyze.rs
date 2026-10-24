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
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

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
    race_then: PathBuf,
    refusal_after_race: PathBuf,
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

        AnalyzeWorkloads {
            hello_race,
            racewrite_nostdlib,
            nanosleep_nocrash,
            race_then,
            refusal_after_race,
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

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.arg("analyze");
    command.args(analyze_opts);
    add_calibrated_skid_margin(&mut command, label);
    command
        .arg(format!("--report-file={}", report_file.display()))
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

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.arg("analyze");
    command.args(analyze_opts);
    add_calibrated_skid_margin(&mut command, label);
    command
        .arg(format!("--report-file={}", report_file.display()))
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
    let guest = &workloads().race_then;
    for seed in 0..40 {
        let mut command = Command::new("timeout");
        command
            .args(["--signal=KILL", "30s"])
            .arg(hermit_test::hermit_binary())
            .args(["run", "--chaos", "--timeout=2"])
            .arg(format!("--seed={seed}"));
        if let Some(margin) = std::env::var_os(ANALYZE_SKID_MARGIN_ENV) {
            command.arg(format!("--skid-margin={}", margin.to_string_lossy()));
        }
        // Match spin's argc and argument length before the race. The mode is
        // inspected only after the winning thread is known.
        command.arg("--").arg(guest).arg("exit");
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
/// With verbose logging the error must name the actual retained trial log.
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

    let dir = tempfile::tempdir().unwrap();
    let workspace_parent = dir.path().join("workspace");
    let report = dir.path().join("report.json");
    let mut command = Command::new("timeout");
    command
        .args(["--signal=KILL", "30s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "analyze",
            "--verbose",
            "--prefix-search",
            "--target-stdout=a-first",
        ])
        .arg(format!("--tmp-dir={}", workspace_parent.display()))
        .arg(format!("--report-file={}", report.display()));
    add_calibrated_skid_margin(&mut command, "analyze refused target");
    command
        .args(["--", "--timeout=3", "--"])
        .arg(&workloads().refusal_after_race)
        .arg("refuse");
    let output = hermit_test::hermit_output("analyze refused target", &mut command);
    let combined = assert_no_refusal_report(&output, &report);
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
/// continuation to try, so analyze fails, saying that the outcome does not
/// depend on a chaos decision and what to run or pass instead.
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
        "no event of the target schedule decides the outcome: without chaos, the scheduler alone \
         already meets the target criteria at a 100000 ns timeslice",
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
