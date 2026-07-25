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
//! explicitly by `validate.sh`. Run them with:
//!
//! ```text
//! cargo test -p hermit --test analyze -- --ignored
//! ```

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
}

fn command_output(mut command: Command, label: &str) -> Output {
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

fn analyze_lock() -> MutexGuard<'static, ()> {
    ANALYZE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

        AnalyzeWorkloads {
            hello_race,
            racewrite_nostdlib,
            nanosleep_nocrash,
        }
    })
}

/// Runs `hermit analyze --search` over `guest` and returns the combined
/// stdout+stderr, asserting a successful exit and that at least one guest stack
/// trace was printed. `expected_output`, when non-empty, must also appear (used
/// to pin the blamed source line).
///
/// Mirrors `tests/util/hermit_analyze_test.sh`: `--analyze-seed=0 --search`,
/// then a chaotic run with a tight preemption timeout so the search has enough
/// scheduling change points to expose the race.
fn run_analyze(label: &str, guest: &Path, analyze_opts: &[&str], expected_output: &str) {
    let _guard = analyze_lock();
    let report_dir = tempfile::tempdir().expect("failed to create analyze report directory");
    let report_file = report_dir.path().join("report.json");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.arg("analyze");
    command.args(analyze_opts);
    command
        .arg(format!("--report-file={}", report_file.display()))
        .args(["--analyze-seed=0", "--search", "--"])
        // Arguments forwarded to the underlying `hermit run` invocations:
        .args(["--chaos", "--summary", "--preemption-timeout=400000", "--"])
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
    if !expected_output.is_empty() {
        assert!(
            combined.contains(expected_output),
            "{label}: expected {expected_output:?} in hermit analyze output\n{combined}"
        );
    }
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
    command
        .arg(format!("--report-file={}", report_file.display()))
        .args(["--analyze-seed=0", "--search", "--"])
        .args(["--chaos", "--summary", "--preemption-timeout=400000", "--"])
        .arg(guest);

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
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
        "",
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

#[test]
#[ignore = "slow: bisecting chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_nanosleep_threads_rejects_indistinguishable_baseline() {
    run_analyze_expect_baseline_collision(
        "analyze nanosleep-threads baseline collision",
        &workloads().nanosleep_nocrash,
        &["--run-arg=--base-env=empty", "--target-exit-code=0"],
    );
}
