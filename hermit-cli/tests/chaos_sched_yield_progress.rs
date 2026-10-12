/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression test for GH #81: chaos mode must not starve sched_yield loops
//! when timer preemption is disabled.
//!
//! Before the fix, a guest whose main thread spins on `sched_yield()` while
//! waiting for a worker thread could hang forever under
//! `--chaos --max-timeslice=disabled`: priorities are fixed at thread
//! creation and only re-randomized at (now-disabled) timer preemptions, so a
//! spinner holding the highest priority monopolized the single logical CPU. The
//! seeds exercised below deterministically reproduced that starvation. The fix
//! turns `sched_yield` into a chaos reprioritization point, so every seed now
//! makes progress and exits cleanly.
//!
//! With timer preemption on, the same loop starved under
//! `--chaos --chaos-target-races` (https://github.com/rrnewton/hermit/issues/4068):
//! the worker can start in the band that runs last, and chaos redraws a band
//! only when a slice expires. There a `sched_yield` now opens a yield epoch:
//! the caller keeps its band but is not selected again until a thread that did
//! not yield has had a turn. The tests below also hold the fix to its limits:
//! a yielder that another thread waits on must not starve, and chaos must
//! still find many schedules for a workload that yields.

#[path = "common/hermit_binary.rs"]
mod hermit_test;
#[path = "common/skid_overshoot.rs"]
mod skid_overshoot;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

/// Chaos seeds that deterministically starved the sched_yield loop before the
/// fix (verified by bisecting the unfixed binary). Any of these hanging is a
/// regression.
const SEEDS: [u64; 4] = [5, 6, 9, 12];

/// Seeds at which the loop starved the worker on main 5e558a6a under
/// `--chaos --chaos-target-races`, with timer preemption on
/// (https://github.com/rrnewton/hermit/issues/4068).
const TARGET_RACES_SEEDS: [u64; 4] = [4, 7, 8, 9];

/// Seeds at which the guest's `--two-spinners` mode starved the worker on main
/// 5e558a6a under `--chaos --chaos-target-races`, and still did with a one-turn
/// yield that leaves the yielder in its own band: the two spinners took turns
/// ahead of the worker.
const TWO_SPINNERS_SEEDS: [u64; 4] = [0, 17, 22, 28];

/// Seeds at which the guest's `--yield-then-write` mode, where the yielder is
/// the thread waited on, finishes on main 5e558a6a under
/// `--chaos --chaos-target-races` but starved with a first version of the fix
/// that moved a yielder to the last runnable band until it was next selected
/// (review of https://github.com/rrnewton/hermit/pull/4080).
const YIELD_THEN_WRITE_SEEDS: [u64; 4] = [0, 4, 7, 9];

/// Seeds for the random-heuristic progress checks: the union of the seeds
/// above. Under `--sched-heuristic=random` they have no special meaning; they
/// are a fixed sample, and a starvation under random hung at several of them
/// in each mode (review of https://github.com/rrnewton/hermit/pull/4201).
const RANDOM_HEURISTIC_SEEDS: [u64; 11] = [0, 4, 5, 6, 7, 8, 9, 12, 17, 22, 28];

/// Chaos seeds for the schedule-diversity check, and the fewest distinct
/// outcomes they must give. Over seeds 0-15, `thread_contention epoll` under
/// `--chaos` gave 16 distinct outcomes on main 5e558a6a, and only 4 with the
/// first version of the fix, whose yields overrode the chaos priority draw.
const DIVERSITY_SEEDS: std::ops::Range<u64> = 0..16;
const MIN_DISTINCT_OUTCOMES: usize = 12;

/// Attempts per run where a typed PMU skid-overshoot refusal, a host property
/// (https://github.com/rrnewton/hermit/issues/1845), is retried.
///
/// Validation runs each test under one CPU budget for the test and its Hermit
/// children (22 CPU-s, `nextest-cpu-wrapper`), and every attempt draws on it.
/// The random-heuristic checks are therefore six tests of 11 runs each, one per
/// chaos setting and guest mode, rather than one test of 66 runs: the 66 runs
/// used 16.3-20.7 CPU-s in 13 validations, and one validation ended at the
/// budget (exit 124) at run 33 of 66, after a skid-refused attempt at seed 28.
/// The costliest group of 11 measured about 10.3 CPU-s.
const SKID_ATTEMPTS: u32 = 3;

/// Per-run timeout. A healthy run finishes in well under a second; a starved
/// run would otherwise spin forever. It stays well inside validation's 22 CPU-s
/// per-test budget, so a hang fails with this test's message naming the seed
/// rather than with the budget's bare exit 124.
const TIMEOUT_SECONDS: u64 = 10;

static GUEST: OnceLock<PathBuf> = OnceLock::new();
static CONTENTION_GUEST: OnceLock<PathBuf> = OnceLock::new();

/// Compile the C guest at `source`, relative to the repository, once.
fn build_guest(cell: &'static OnceLock<PathBuf>, source: &str, extra: &[&str]) -> &'static Path {
    cell.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("chaos-sched-yield-progress");
        fs::create_dir_all(&build_root).expect("failed to create build directory");
        let source = repository.join(source);
        let output = build_root.join(source.file_stem().expect("a C source file"));
        let mut command = Command::new("cc");
        command
            .args(["-std=c11", "-O2", "-g", "-pthread"])
            .args(extra)
            .args(["-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&output);
        let status = command
            .status()
            .unwrap_or_else(|error| panic!("failed to run cc for {}: {error}", source.display()));
        assert!(status.success(), "guest compilation failed: {command:?}");
        output
    })
}

fn guest() -> &'static Path {
    build_guest(&GUEST, "tests/c/sched_yield_progress.c", &["-D_GNU_SOURCE"])
}

/// The repository's chaos schedule-diversity guest, which defines
/// `_GNU_SOURCE` itself.
fn contention_guest() -> &'static Path {
    build_guest(
        &CONTENTION_GUEST,
        "tests/e2e/determinism-stress/thread_contention.c",
        &[],
    )
}

fn run_seed(seed: u64, chaos_args: &[&str], guest_args: &[&str]) {
    let stdout = run_chaos(guest(), seed, chaos_args, guest_args);
    assert!(
        stdout.contains("sched-yield-progress-ok"),
        "missing progress marker under chaos with seed {seed}; stdout:\n{stdout}"
    );
}

/// Run `guest` under `--chaos` with `seed`, require a clean exit within the
/// timeout, and return its stdout.
fn run_chaos(guest: &Path, seed: u64, chaos_args: &[&str], guest_args: &[&str]) -> String {
    let (output, rendered) = chaos_output(guest, seed, &[], chaos_args, guest_args);
    let output = checked_chaos_output(output, seed, &rendered);
    String::from_utf8(output.stdout).expect("guest stdout should be UTF-8")
}

/// [`run_chaos`] with `hermit_args` before `run`, running again only after a
/// typed PMU skid-overshoot refusal, and returning the whole output.
fn run_chaos_retrying_skid(
    guest: &Path,
    seed: u64,
    hermit_args: &[&str],
    chaos_args: &[&str],
    guest_args: &[&str],
) -> Output {
    let name = format!("seed {seed} {chaos_args:?} {guest_args:?}");
    let (_, output, rendered) =
        skid_overshoot::retry_typed_skid_refusal(&name, SKID_ATTEMPTS, || {
            chaos_output(guest, seed, hermit_args, chaos_args, guest_args)
        });
    checked_chaos_output(output, seed, &rendered)
}

/// Run `guest` once under `--chaos` with `seed`, and return its output with
/// the rendered command.
fn chaos_output(
    guest: &Path,
    seed: u64,
    hermit_args: &[&str],
    chaos_args: &[&str],
    guest_args: &[&str],
) -> (Output, String) {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(hermit_test::hermit_binary())
        .args(hermit_args)
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--chaos",
        ])
        .args(chaos_args)
        .arg(format!("--seed={seed}"))
        .arg("--")
        .arg(guest)
        .args(guest_args);

    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start guest (seed {seed}): {rendered}: {error}"));
    (output, rendered)
}

/// Require that a chaos run exited cleanly within the timeout.
fn checked_chaos_output(output: Output, seed: u64, rendered: &str) -> Output {
    // `timeout` exits 124 when it has to kill the child; that is exactly the
    // starvation symptom this test guards against.
    assert_ne!(
        output.status.code(),
        Some(124),
        "sched_yield loop starved (timed out) under chaos with seed {seed}: {rendered}"
    );
    assert!(
        output.status.success(),
        "guest failed under chaos with seed {seed}: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

#[test]
fn chaos_sched_yield_makes_progress_without_timer_preemption() {
    for seed in SEEDS {
        run_seed(seed, &["--max-timeslice=disabled"], &[]);
    }
}

#[test]
fn targeted_chaos_sched_yield_makes_progress_with_timer_preemption() {
    for seed in TARGET_RACES_SEEDS {
        run_seed(seed, &["--chaos-target-races"], &[]);
    }
    for seed in TWO_SPINNERS_SEEDS {
        run_seed(seed, &["--chaos-target-races"], &["--two-spinners"]);
    }
}

/// A thread that yields and is then waited on by threads that never yield
/// still runs.
#[test]
fn targeted_chaos_does_not_starve_a_yielder_that_is_waited_on() {
    for seed in YIELD_THEN_WRITE_SEEDS {
        run_seed(seed, &["--chaos-target-races"], &["--yield-then-write"]);
    }
}

/// Under `--sched-heuristic=random`, the heuristic every chaos cell of the E2E
/// manifest uses, a `sched_yield` stays a weak yield rather than opening an
/// epoch (https://github.com/rrnewton/hermit/issues/4150). The uniform pick at
/// each turn must still reach the thread a spinner waits on, at every seed in
/// [`RANDOM_HEURISTIC_SEEDS`]. The six tests below are one per chaos setting
/// and guest mode, so each has its own CPU budget ([`SKID_ATTEMPTS`]).
fn random_chaos_makes_progress(chaos_args: &[&str], guest_args: &[&str]) {
    for seed in RANDOM_HEURISTIC_SEEDS {
        let output = run_chaos_retrying_skid(guest(), seed, &[], chaos_args, guest_args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("sched-yield-progress-ok"),
            "missing progress marker under chaos {chaos_args:?} {guest_args:?} with seed {seed}; stdout:\n{stdout}"
        );
    }
}

const RANDOM: &[&str] = &["--sched-heuristic=random"];
const TARGETED_RANDOM: &[&str] = &["--sched-heuristic=random", "--chaos-target-races"];

#[test]
fn random_chaos_sched_yield_lets_the_worker_run() {
    random_chaos_makes_progress(RANDOM, &[]);
}

#[test]
fn random_chaos_sched_yield_lets_the_worker_run_past_two_spinners() {
    random_chaos_makes_progress(RANDOM, &["--two-spinners"]);
}

#[test]
fn random_chaos_runs_a_yielder_that_is_waited_on() {
    random_chaos_makes_progress(RANDOM, &["--yield-then-write"]);
}

#[test]
fn targeted_random_chaos_sched_yield_lets_the_worker_run() {
    random_chaos_makes_progress(TARGETED_RANDOM, &[]);
}

#[test]
fn targeted_random_chaos_sched_yield_lets_the_worker_run_past_two_spinners() {
    random_chaos_makes_progress(TARGETED_RANDOM, &["--two-spinners"]);
}

#[test]
fn targeted_random_chaos_runs_a_yielder_that_is_waited_on() {
    random_chaos_makes_progress(TARGETED_RANDOM, &["--yield-then-write"]);
}

/// The scheduler commits in an INFO log whose resources begin with `resource`.
fn commits_of(log: &str, resource: &str) -> usize {
    let needle = format!("using resources {{{resource}");
    log.lines()
        .filter(|line| line.contains("COMMIT turn") && line.contains(&needle))
        .count()
}

/// The route a chaos `sched_yield` takes, read from the scheduler's commits.
/// Under `--sched-heuristic=random` the yield is the weak `SleepUntil(0)`
/// yield (https://github.com/rrnewton/hermit/issues/4150); under the default
/// heuristic it is still a `SchedYield`, which opens a yield epoch
/// (https://github.com/rrnewton/hermit/issues/4068). The `--yield-then-write`
/// guest's main thread yields exactly once whatever the schedule; the
/// `--two-spinners` guest yields only while its worker has not finished, which
/// at a fixed seed differed between this host's build and the validation root.
#[test]
fn chaos_sched_yield_route_depends_on_the_heuristic() {
    for (chaos_args, weak) in [(&["--sched-heuristic=random"][..], true), (&[][..], false)] {
        let output = run_chaos_retrying_skid(
            guest(),
            4,
            &["--log=info"],
            chaos_args,
            &["--yield-then-write"],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let log = String::from_utf8_lossy(&output.stderr);
        let commits: Vec<&str> = log
            .lines()
            .filter(|line| line.contains("COMMIT turn"))
            .collect();
        let yields = log.matches("inbound syscall: sched_yield").count();
        let weak_yields = commits_of(&log, "SleepUntil(LogicalTime(0))");
        let sched_yields = commits_of(&log, "SchedYield");
        assert!(
            stdout.contains("yielded") && stdout.contains("sched-yield-progress-ok"),
            "the guest did not yield and finish under chaos {chaos_args:?}; stdout:\n{stdout}"
        );
        let expected = if weak { (1, 1, 0) } else { (1, 0, 1) };
        assert_eq!(
            (yields, weak_yields, sched_yields),
            expected,
            "under chaos {chaos_args:?}: (sched_yield calls, SleepUntil(0) commits, \
             SchedYield commits); the run's commits:\n{}",
            commits.join("\n")
        );
    }
}

/// A yield must not override the chaos priority draw: on a workload whose
/// threads yield before they write, `--chaos` still reaches many write orders.
#[test]
fn chaos_keeps_its_schedule_diversity_on_a_yielding_workload() {
    let outcomes: std::collections::BTreeSet<String> = DIVERSITY_SEEDS
        .map(|seed| run_chaos(contention_guest(), seed, &[], &["epoll"]))
        .collect();
    assert!(
        outcomes.len() >= MIN_DISTINCT_OUTCOMES,
        "{} distinct outcomes over seeds {DIVERSITY_SEEDS:?}, want at least {MIN_DISTINCT_OUTCOMES}: {outcomes:#?}",
        outcomes.len()
    );
}

/// The scheduler turns, syscalls and virtual time a run's summary reports.
fn schedule_summary(path: &Path) -> (u64, u64, u64) {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let summary: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()));
    let field = |name: &str| {
        summary[name]
            .as_u64()
            .unwrap_or_else(|| panic!("{} has no integer {name}: {text}", path.display()))
    };
    (
        field("sched_turns"),
        field("syscalls"),
        field("virttime_elapsed"),
    )
}

/// How many priority changes a preemption record holds, over all threads.
fn recorded_preemption_points(path: &Path) -> usize {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let record: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()));
    record["per_thread"]
        .as_object()
        .unwrap_or_else(|| panic!("{} has no per_thread map", path.display()))
        .values()
        .map(|thread| thread["prio_changes"].as_array().map_or(0, Vec::len))
        .sum()
}

/// Preemption replay of two pinned chaos records of this guest reaches the
/// recorded schedule: the same scheduler turns, syscalls, virtual time and
/// output. A chaos `sched_yield` changes no priority, draws nothing from the
/// chaos PRNG and consumes no recorded preemption point, so its yield epoch
/// must not move a replay off its record. Limits: the record must hold
/// preemption points, but the test does not show one inside a yielding
/// interval, and it compares a summary, not the schedule. Replay identity in
/// general is blocked by a pre-existing defect: a slice end recorded just
/// after a syscall's commit can replay just before it, with or without
/// yields (https://github.com/rrnewton/hermit/issues/4082), so these seeds
/// are the ones that avoid it.
#[test]
fn targeted_chaos_preemption_replay_matches_two_pinned_records() {
    for seed in [4, 7] {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("chaos-yield-replay-{seed}"));
        fs::create_dir_all(&dir).expect("failed to create the replay directory");
        let preemptions = dir.join("preemptions.json");
        let _ = fs::remove_file(&preemptions);
        let mut results = Vec::new();
        for (phase, option) in [
            (
                "record",
                format!("--record-preemptions-to={}", preemptions.display()),
            ),
            (
                "replay",
                format!("--replay-preemptions-from={}", preemptions.display()),
            ),
        ] {
            let summary = dir.join(format!("{phase}.summary.json"));
            let mut command = Command::new("timeout");
            command
                .arg("--kill-after=2s")
                .arg(format!("{TIMEOUT_SECONDS}s"))
                .arg(hermit_test::hermit_binary())
                .args([
                    "run",
                    "--base-env=minimal",
                    "--no-virtualize-cpuid",
                    "--chaos",
                    "--chaos-target-races",
                    "--max-timeslice=300000",
                ])
                .arg(format!("--seed={seed}"))
                .arg(&option)
                .arg(format!("--summary-json={}", summary.display()))
                .arg("--")
                .arg(guest());

            hermit_test::configure_guest_execution(&mut command);
            let rendered = format!("{command:?}");
            let output = command.output().unwrap_or_else(|error| {
                panic!("failed to start chaos {phase}: {rendered}: {error}")
            });
            assert!(
                output.status.success(),
                "chaos {phase} failed with seed {seed}: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            let stdout = String::from_utf8(output.stdout).expect("guest stdout should be UTF-8");
            results.push((stdout, schedule_summary(&summary)));
        }
        assert!(
            recorded_preemption_points(&preemptions) > 0,
            "seed {seed}: the record holds no preemption point"
        );
        assert_eq!(
            results[0], results[1],
            "seed {seed}: replay left the record"
        );
    }
}

fn run_strict_guest(args: &[&str]) {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(hermit_test::hermit_binary())
        .args([
            "run",
            "--strict",
            "--verify",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--",
        ])
        .arg(guest())
        .args(args);

    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start strict guest: {rendered}: {error}"));

    assert_ne!(
        output.status.code(),
        Some(124),
        "sched_yield guest timed out under strict verify: {rendered}"
    );
    assert!(
        output.status.success(),
        "sched_yield guest failed under strict verify: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn strict_sched_yield_is_deterministic() {
    run_strict_guest(&[]);
}

#[test]
fn strict_vfork_child_sched_yield_is_deterministic() {
    run_strict_guest(&["--vfork"]);
}

#[test]
fn preemption_replay_preserves_vfork_sched_yield_progress() {
    let schedule = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sched-yield-preemptions.json");
    let _ = fs::remove_file(&schedule);

    for (phase, option) in [
        (
            "record",
            format!("--record-preemptions-to={}", schedule.display()),
        ),
        (
            "replay",
            format!("--replay-preemptions-from={}", schedule.display()),
        ),
    ] {
        let mut command = Command::new("timeout");
        command
            .arg("--kill-after=2s")
            .arg(format!("{TIMEOUT_SECONDS}s"))
            .arg(hermit_test::hermit_binary())
            .args([
                "run",
                "--strict",
                "--preemption-timeout=disabled",
                &option,
                "--base-env=minimal",
                "--no-virtualize-cpuid",
                "--",
            ])
            .arg(guest())
            .arg("--vfork");

        hermit_test::configure_guest_execution(&mut command);
        let rendered = format!("{command:?}");
        let output = command.output().unwrap_or_else(|error| {
            panic!("failed to start preemption {phase}: {rendered}: {error}")
        });
        assert_ne!(
            output.status.code(),
            Some(124),
            "preemption {phase} timed out: {rendered}"
        );
        assert!(
            output.status.success(),
            "preemption {phase} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
