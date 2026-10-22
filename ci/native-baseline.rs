#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Native (no-Hermit) baselines and Hermit slowdown statistics.
//!
//! Hermit overhead conflates two different costs: instrumentation itself, and
//! sequentializing a parallel guest onto one core. This tool measures the
//! native side of that split. For each selected manifest test it runs the
//! guest exactly as the harness prepares it (`runner::prepare_test`), once
//! pinned to a single core and once unpinned, and records wall time plus
//! whether the guest was observed multi-threaded or multi-process. Joining
//! those rows with retained Hermit `CellResult` rows gives, per backend, the
//! slowdown distribution over completed cells, and for every cell that timed
//! out under Hermit, the minimum slowdown its budget implies
//! (`budget / single-core native time`): a cell whose budget is only 2x its
//! single-core native time is plausibly just slow, while one whose budget is
//! 100x is plausibly stuck.
//!
//! ```cargo
//! [dependencies]
//! hermit-manifest-plan = { path = "manifest-plan" }
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! ```
//!
//! Usage: see `--help`. Rows are `native-baseline/v1` JSONL; see
//! [`NativeRow`]. Heavy runs should be launched at `nice 19` by the caller;
//! this tool never starts Hermit.

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Stdio;
use std::time::Instant;

use hermit_manifest_plan::host_capability::HostCapabilities;
use hermit_manifest_plan::runner::CellResult;
use hermit_manifest_plan::runner::ManifestSet;
use hermit_manifest_plan::runner::Population;
use hermit_manifest_plan::runner::RunContext;
use hermit_manifest_plan::runner::ScheduledWorkerCapacity;
use hermit_manifest_plan::runner::SelectedCell;
use hermit_manifest_plan::runner::Selection;
use hermit_manifest_plan::runner::prepare_test;
use hermit_manifest_plan::timeouts::TimeoutMultipliers;
use serde::Deserialize;
use serde::Serialize;

const ROW_SCHEMA: &str = "native-baseline/v1";
const STATS_SCHEMA: &str = "native-baseline-stats/v1";

const HELP: &str = "\
Native baselines and Hermit slowdown statistics

Usage: ci/native-baseline.rs COMMAND [OPTIONS]

Commands:
  run       Run selected manifest guests natively (pinned + unpinned)
  stats     Join native rows with Hermit rows; print slowdown statistics
  self-test Check the statistics math and row round-trip (runs no guest)

Every command accepts -h and --help. Exit status is 0 on success, 1 when a
selected guest or row failed, and 2 for a usage error (the message names the
remedy). Guest exit codes 126/127 are invocation failures and are recorded
as failures, never as passes.

run options:
  --repo-root DIR     Checkout holding tests/e2e/manifests (default: cwd)
  --test ID           Manifest test ID; may repeat (default: whole verify corpus)
  --cells-file PATH   Newline-delimited manifest test IDs to select
  --timeouts-from P   Retained CellResult JSONL; select the distinct tests
                      whose rows have error_kind cpu-timeout or wall-timeout
  --out PATH          JSONL output file (required)
  --work-root DIR     Guest preparation scratch (default: <repo>/ignored/native-baseline)
  --repetitions N     Measured repetitions per pinning (default: 3)
  --pinning MODE      both | single | unpinned (default: both)
  --pin-mechanism M   taskset (default, ~0 launch overhead) or cpuset-alloc
                      (hard AllowedCPUs pin, ~1 s launch overhead here)
  --pin-cpu CPU       Single core to pin to (default: first allowed core)
  --timeout-seconds N Per-repetition native cap (default: 300)
  --prepare-compat-fixtures
                      Materialize validate's compat fixtures once (the
                      compatprep step: tests/compat/prepare_real_compat_
                      fixtures.sh into <run-state>/strict-compat/real-
                      compat-fixtures) and point VALIDATE_RUN_STATE at it,
                      so compat guests whose argv names that state can be
                      timed. Ignored when VALIDATE_RUN_STATE is already set.
  --run-state DIR     Use DIR as VALIDATE_RUN_STATE without preparing
                      fixtures (ignored when the variable is already set)

stats options:
  --native PATH       native-baseline/v1 JSONL from `run` (required)
  --hermit PATH       Retained CellResult JSONL (required; may repeat)
  --out PATH          Also write the machine-readable report as JSON

Environment:
  CPUSET_ALLOC        Path to cpuset-alloc; used only with
                      `--pin-mechanism cpuset-alloc` (systemd AllowedCPUs
                      hard pin). The default timed pin is `taskset`.
                      The row records which mechanism ran.
";

fn usage_error(message: &str) -> ExitCode {
    eprintln!("native-baseline: {message}");
    eprintln!("native-baseline: run `ci/native-baseline.rs --help` for usage");
    ExitCode::from(2)
}

fn fail(message: &str) -> ExitCode {
    eprintln!("native-baseline: {message}");
    ExitCode::from(1)
}

/// One native measurement of one manifest test under one pinning.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct NativeRow {
    schema: String,
    test: String,
    category: String,
    lane: String,
    /// `single` (one core) or `unpinned`.
    pinning: String,
    /// Mechanism that established the pinning: `cpuset-alloc`, `taskset`,
    /// or `none` for the unpinned run.
    pin_mechanism: String,
    /// The single core for `single`, empty for `unpinned`.
    cpu_set: Vec<usize>,
    /// Every measured repetition's wall time, milliseconds.
    samples_ms: Vec<f64>,
    median_ms: f64,
    min_ms: f64,
    /// Guest exit code per repetition; `null` when killed by a signal or
    /// by this tool's native cap.
    exit_codes: Vec<Option<i32>>,
    /// True when any repetition exceeded the native cap and was killed.
    timed_out: bool,
    max_threads: u32,
    max_processes: u32,
    multi_threaded: bool,
    multi_process: bool,
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    }
}

/// Nearest-rank percentile (p in 0..=100).
fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn first_allowed_cpu() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let list = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))?
        .trim();
    let first = list.split(',').next()?;
    first.split('-').next()?.trim().parse::<usize>().ok()
}

fn find_cpuset_alloc() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CPUSET_ALLOC") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join("cpuset-alloc"))
        .find(|candidate| candidate.is_file())
}

struct GuestRun {
    wall_ms: f64,
    exit_code: Option<i32>,
    timed_out: bool,
    max_threads: u32,
    max_processes: u32,
}

/// Map every live pid to its parent, from one /proc scan.
fn parent_map() -> BTreeMap<u32, u32> {
    let mut parents = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // Field 4 (ppid) follows the parenthesised comm.
            if let Some((_, rest)) = stat.rsplit_once(')') {
                if let Some(ppid) = rest
                    .split_whitespace()
                    .nth(1)
                    .and_then(|ppid| ppid.parse::<u32>().ok())
                {
                    parents.insert(pid, ppid);
                }
            }
        }
    }
    parents
}

/// All descendants of `root` (including `root`) from a parent map.
fn descendant_tree(parents: &BTreeMap<u32, u32>, root: u32) -> Vec<u32> {
    let mut tree = vec![root];
    let mut index = 0;
    while index < tree.len() {
        let parent = tree[index];
        index += 1;
        for (&pid, &ppid) in parents {
            if ppid == parent && !tree.contains(&pid) {
                tree.push(pid);
            }
        }
    }
    tree
}

fn thread_count(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .and_then(|value| value.trim().parse::<u32>().ok())
}

/// Run one guest repetition, sampling the whole descendant tree's thread
/// and process counts while it runs. Sampling is observational only.
///
/// The timed single-core pin defaults to `taskset`: `cpuset-alloc run`
/// measured 0.96 s of launch overhead on this host (cargo-runner cache
/// check plus systemd scope creation), which would swamp sub-second
/// guests. `--pin-mechanism cpuset-alloc` selects the hard
/// `AllowedCPUs` pin instead; the row records which mechanism ran.
fn run_guest_once(
    argv: &[String],
    cwd: &Path,
    pinning: &str,
    pin_cpu: usize,
    pin_mechanism: &str,
    cpuset_alloc: Option<&Path>,
    timeout_seconds: u64,
) -> Result<GuestRun, String> {
    let mut command = if pinning == "single" && pin_mechanism == "cpuset-alloc" {
        let alloc = cpuset_alloc.ok_or_else(|| {
            "--pin-mechanism cpuset-alloc requires cpuset-alloc on PATH or CPUSET_ALLOC set"
                .to_string()
        })?;
        let mut cmd = Command::new(alloc);
        cmd.args(["run", "--cores", "1", "--"]).args(argv);
        cmd
    } else if pinning == "single" {
        let mut cmd = Command::new("taskset");
        cmd.args(["-c", &pin_cpu.to_string()]).args(argv);
        cmd
    } else {
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd
    };
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot launch {:?}: {error}", argv.first()))?;
    let pid = child.id();
    let mut max_threads = 1u32;
    let mut max_processes = 1u32;
    let mut timed_out = false;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if started.elapsed().as_secs() >= timeout_seconds {
            timed_out = true;
            let _ = child.kill();
            break;
        }
        // Sample the whole descendant tree: the launched pid may be a
        // wrapper (taskset, systemd-run, or a shell guest) whose own
        // thread count says nothing about the guest.
        let tree = descendant_tree(&parent_map(), pid);
        max_processes = max_processes.max(tree.len() as u32);
        for member in &tree {
            if let Some(threads) = thread_count(*member) {
                max_threads = max_threads.max(threads);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let status = child.wait().map_err(|error| error.to_string())?;
    Ok(GuestRun {
        wall_ms: started.elapsed().as_secs_f64() * 1000.0,
        exit_code: status.code(),
        timed_out,
        max_threads,
        max_processes,
    })
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path, what: &str) -> Result<Vec<T>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {what} {}: {error}", path.display()))?;
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(serde_json::from_str(line).map_err(|error| {
            format!(
                "{}:{}: invalid {what} row: {error}",
                path.display(),
                index + 1
            )
        })?);
    }
    Ok(rows)
}

fn timeout_test_ids(hermit_rows: &[CellResult]) -> BTreeSet<String> {
    hermit_rows
        .iter()
        .filter(|row| {
            matches!(
                row.error_kind.as_deref(),
                Some("cpu-timeout" | "wall-timeout")
            )
        })
        .map(|row| row.test.clone())
        .collect()
}

struct RunOptions {
    repo_root: PathBuf,
    tests: BTreeSet<String>,
    out: PathBuf,
    work_root: PathBuf,
    repetitions: usize,
    pinning: String,
    pin_mechanism: String,
    pin_cpu: Option<usize>,
    timeout_seconds: u64,
    prepare_compat_fixtures: bool,
    run_state: Option<PathBuf>,
}

/// The `VALIDATE_RUN_STATE` a run should use, or `None` to leave the
/// environment untouched: an already-set variable wins, then an explicit
/// `--run-state`, then the `--prepare-compat-fixtures` default under the
/// work root.
fn resolve_run_state(
    prepare_compat_fixtures: bool,
    explicit: Option<&Path>,
    work_root: &Path,
    env_set: bool,
) -> Option<PathBuf> {
    if env_set {
        return None;
    }
    if let Some(dir) = explicit {
        return Some(dir.to_path_buf());
    }
    prepare_compat_fixtures.then(|| work_root.join("run-state"))
}

fn parse_run_options(args: &[String]) -> Result<RunOptions, String> {
    let mut repo_root: Option<PathBuf> = None;
    let mut tests: BTreeSet<String> = BTreeSet::new();
    let mut cells_file: Option<PathBuf> = None;
    let mut timeouts_from: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut work_root: Option<PathBuf> = None;
    let mut repetitions = 3usize;
    let mut pinning = "both".to_string();
    let mut pin_mechanism = "taskset".to_string();
    let mut pin_cpu: Option<usize> = None;
    let mut timeout_seconds = 300u64;
    let mut prepare_compat_fixtures = false;
    let mut run_state: Option<PathBuf> = None;
    let mut i = 0;
    let take = |args: &[String], i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("{flag} requires a value; see --help"))
    };
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--repo-root" => repo_root = Some(PathBuf::from(take(args, &mut i, "--repo-root")?)),
            "--test" => {
                tests.insert(take(args, &mut i, "--test")?);
            }
            "--cells-file" => cells_file = Some(PathBuf::from(take(args, &mut i, "--cells-file")?)),
            "--timeouts-from" => {
                timeouts_from = Some(PathBuf::from(take(args, &mut i, "--timeouts-from")?))
            }
            "--out" => out = Some(PathBuf::from(take(args, &mut i, "--out")?)),
            "--work-root" => work_root = Some(PathBuf::from(take(args, &mut i, "--work-root")?)),
            "--repetitions" => {
                let raw = take(args, &mut i, "--repetitions")?;
                repetitions = raw
                    .parse()
                    .map_err(|_| format!("--repetitions {raw:?} is not a positive integer"))?;
                if repetitions == 0 {
                    return Err("--repetitions must be at least 1".into());
                }
            }
            "--pinning" => {
                pinning = take(args, &mut i, "--pinning")?;
                if !matches!(pinning.as_str(), "both" | "single" | "unpinned") {
                    return Err(format!(
                        "--pinning {pinning:?} is not both, single, or unpinned"
                    ));
                }
            }
            "--pin-mechanism" => {
                pin_mechanism = take(args, &mut i, "--pin-mechanism")?;
                if !matches!(pin_mechanism.as_str(), "taskset" | "cpuset-alloc") {
                    return Err(format!(
                        "--pin-mechanism {pin_mechanism:?} is not taskset or cpuset-alloc"
                    ));
                }
            }
            "--pin-cpu" => {
                let raw = take(args, &mut i, "--pin-cpu")?;
                pin_cpu = Some(
                    raw.parse()
                        .map_err(|_| format!("--pin-cpu {raw:?} is not a CPU id"))?,
                );
            }
            "--timeout-seconds" => {
                let raw = take(args, &mut i, "--timeout-seconds")?;
                timeout_seconds = raw
                    .parse()
                    .map_err(|_| format!("--timeout-seconds {raw:?} is not a positive integer"))?;
                if timeout_seconds == 0 {
                    return Err("--timeout-seconds must be at least 1".into());
                }
            }
            "--prepare-compat-fixtures" => prepare_compat_fixtures = true,
            "--run-state" => run_state = Some(PathBuf::from(take(args, &mut i, "--run-state")?)),
            other => return Err(format!("unknown run option {other:?}; see --help")),
        }
        i += 1;
    }
    let repo_root = repo_root.unwrap_or_else(|| PathBuf::from("."));
    if let Some(path) = &cells_file {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read --cells-file {}: {error}", path.display()))?;
        tests.extend(
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string),
        );
    }
    if let Some(path) = &timeouts_from {
        let rows: Vec<CellResult> = read_jsonl(path, "CellResult")?;
        tests.extend(timeout_test_ids(&rows));
    }
    let out = out.ok_or_else(|| "--out PATH is required for run; see --help".to_string())?;
    let work_root = work_root.unwrap_or_else(|| repo_root.join("ignored/native-baseline"));
    Ok(RunOptions {
        repo_root,
        tests,
        out,
        work_root,
        repetitions,
        pinning,
        pin_mechanism,
        pin_cpu,
        timeout_seconds,
        prepare_compat_fixtures,
        run_state,
    })
}

fn cmd_run(args: &[String]) -> ExitCode {
    let options = match parse_run_options(args) {
        Ok(options) => options,
        Err(error) => return usage_error(&error),
    };
    // Compat guests name validate's run state in their argv; materialize
    // it with validate's own compatprep command when asked to.
    if let Some(state_dir) = resolve_run_state(
        options.prepare_compat_fixtures,
        options.run_state.as_deref(),
        &options.work_root,
        std::env::var_os("VALIDATE_RUN_STATE").is_some(),
    ) {
        if options.prepare_compat_fixtures {
            let fixtures = state_dir.join("strict-compat").join("real-compat-fixtures");
            if !fixtures.is_dir() {
                let script = options
                    .repo_root
                    .join("tests/compat/prepare_real_compat_fixtures.sh");
                let status = Command::new("bash")
                    .arg(&script)
                    .arg(&fixtures)
                    .current_dir(&options.repo_root)
                    .status();
                match status {
                    Ok(status) if status.success() => {}
                    Ok(status) => {
                        return fail(&format!(
                            "compat fixture preparation exited {status}; run {} {} manually to see why",
                            script.display(),
                            fixtures.display()
                        ));
                    }
                    Err(error) => {
                        return fail(&format!(
                            "cannot run compat fixture preparation {}: {error}",
                            script.display()
                        ));
                    }
                }
            }
        }
        // SAFETY: set before any guest preparation reads the variable;
        // this process has spawned no threads yet at this point.
        unsafe { std::env::set_var("VALIDATE_RUN_STATE", &state_dir) };
    }
    let manifests = match ManifestSet::load(&options.repo_root) {
        Ok(manifests) => manifests,
        Err(error) => {
            return fail(&format!(
                "cannot load manifests under {}: {error}; pass --repo-root DIR naming a Hermit checkout",
                options.repo_root.display()
            ));
        }
    };
    let cells = match manifests.select(&Selection {
        population: Some(Population::Enabled),
        include_occasional: true,
        include_manual: true,
        all_run_types: true,
        mode: Some("verify".into()),
        ..Selection::default()
    }) {
        Ok(cells) => cells,
        Err(error) => return fail(&format!("cannot select verify cells: {error}")),
    };
    // One representative verify cell per test (ptrace preferred: its guest
    // argv is the canonical one the other backends share).
    let mut representative: BTreeMap<String, SelectedCell> = BTreeMap::new();
    for cell in cells {
        if !options.tests.is_empty() && !options.tests.contains(&cell.id.test) {
            continue;
        }
        let replace = match representative.get(&cell.id.test) {
            None => true,
            Some(existing) => {
                existing.id.backend.as_deref() != Some("ptrace")
                    && cell.id.backend.as_deref() == Some("ptrace")
            }
        };
        if replace {
            representative.insert(cell.id.test.clone(), cell);
        }
    }
    if representative.is_empty() {
        return usage_error(
            "no verify cells matched the selection; check --test IDs against `test-harness plan`",
        );
    }
    let context = RunContext {
        root: options.repo_root.clone(),
        hermit_bin: options.repo_root.join("target/debug/hermit"),
        result_root: options.work_root.join("results"),
        build_root: options.work_root.join("build"),
        run_id: "native-baseline".into(),
        machine_shortname: String::new(),
        kernel_version: String::new(),
        host_capabilities: HostCapabilities::default(),
        exact_branch_counter: None,
        attempt: 1,
        run_index: None,
        epoch: "2026-01-01T00:00:00Z".into(),
        source_sha: "0".repeat(40),
        source_dirty: false,
        binary_build_sha: None,
        prebuilt: false,
        keep_logs: false,
        parity_retained: BTreeSet::new(),
        run_verify_strict: false,
        record_verify_strict: false,
        timeout_multipliers: TimeoutMultipliers::default(),
        scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
        isolated_workdir: None,
    };
    let pin_cpu = match options.pin_cpu.or_else(first_allowed_cpu) {
        Some(cpu) => cpu,
        None => {
            return fail(
                "cannot determine an allowed CPU from /proc/self/status; pass --pin-cpu CPU",
            );
        }
    };
    let cpuset_alloc = find_cpuset_alloc();
    let pinnings: Vec<&str> = match options.pinning.as_str() {
        "single" => vec!["single"],
        "unpinned" => vec!["unpinned"],
        _ => vec!["single", "unpinned"],
    };
    if let Some(parent) = options.out.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let mut output = match std::fs::File::create(&options.out) {
        Ok(file) => file,
        Err(error) => {
            return fail(&format!(
                "cannot create --out {}: {error}",
                options.out.display()
            ));
        }
    };
    let mut failures = 0usize;
    for (test, cell) in &representative {
        let dir = options.work_root.join(test.replace('/', "-"));
        let guest = match prepare_test(&context, cell, &dir) {
            Ok(guest) => guest,
            Err(error) => {
                eprintln!("native-baseline: {test}: guest preparation failed: {error}");
                failures += 1;
                continue;
            }
        };
        for pinning in &pinnings {
            let mut samples_ms = Vec::new();
            let mut exit_codes: Vec<Option<i32>> = Vec::new();
            let mut timed_out = false;
            let mut max_threads = 1u32;
            let mut max_processes = 1u32;
            for _ in 0..options.repetitions {
                match run_guest_once(
                    &guest,
                    &options.repo_root,
                    pinning,
                    pin_cpu,
                    &options.pin_mechanism,
                    cpuset_alloc.as_deref(),
                    options.timeout_seconds,
                ) {
                    Ok(run) => {
                        samples_ms.push(run.wall_ms);
                        exit_codes.push(run.exit_code);
                        timed_out |= run.timed_out;
                        max_threads = max_threads.max(run.max_threads);
                        max_processes = max_processes.max(run.max_processes);
                    }
                    Err(error) => {
                        eprintln!("native-baseline: {test} ({pinning}): {error}");
                        failures += 1;
                    }
                }
            }
            if samples_ms.is_empty() {
                continue;
            }
            // 126/127 are invocation failures, never passes.
            if exit_codes
                .iter()
                .any(|code| matches!(code, Some(126 | 127)))
            {
                failures += 1;
            }
            let row = NativeRow {
                schema: ROW_SCHEMA.into(),
                test: test.clone(),
                category: cell.category.clone(),
                lane: cell.test.lane.clone(),
                pinning: (*pinning).to_string(),
                pin_mechanism: if *pinning == "single" {
                    options.pin_mechanism.clone()
                } else {
                    "none".into()
                },
                cpu_set: if *pinning == "single" {
                    vec![pin_cpu]
                } else {
                    Vec::new()
                },
                median_ms: median(&samples_ms),
                min_ms: samples_ms.iter().copied().fold(f64::INFINITY, f64::min),
                samples_ms,
                exit_codes,
                timed_out,
                max_threads,
                max_processes,
                multi_threaded: max_threads > 1,
                multi_process: max_processes > 1,
            };
            let line = serde_json::to_string(&row).unwrap_or_default();
            if writeln!(output, "{line}").is_err() {
                return fail("cannot write --out rows");
            }
            println!(
                "{} {}: median {:.1} ms ({} sample(s))",
                row.test,
                row.pinning,
                row.median_ms,
                row.samples_ms.len()
            );
        }
    }
    if failures > 0 {
        eprintln!(
            "native-baseline: {failures} guest(s) failed; rows in {} cover the rest",
            options.out.display()
        );
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

#[derive(Debug, Serialize)]
struct BackendStats {
    backend: String,
    /// Completed (PASS with a measured duration) verify cells joined.
    cells: usize,
    mean: f64,
    median: f64,
    p90: f64,
}

#[derive(Debug, Serialize)]
struct ImpliedSlowdown {
    test: String,
    mode: String,
    backend: String,
    error_kind: String,
    budget_seconds: f64,
    native_single_median_ms: f64,
    /// `budget_seconds / (native_single_median_ms / 1000)`.
    implied_min_slowdown: f64,
}

#[derive(Debug, Serialize)]
struct StatsReport {
    schema: String,
    /// Slowdown denominator: single-core native median per test.
    baseline: String,
    backends: Vec<BackendStats>,
    implied: Vec<ImpliedSlowdown>,
    /// Hermit verify rows with no single-core native row for their test.
    hermit_rows_without_native: usize,
    /// Timeout rows whose test has no single-core native row.
    timeout_rows_without_native: usize,
}

/// Pure join: per-backend slowdown over completed verify cells, plus the
/// implied minimum slowdown for each timeout verify cell. Kept free of IO
/// so `self-test` exercises exactly the shipped math.
fn compute_stats(native_rows: &[NativeRow], hermit_rows: &[CellResult]) -> StatsReport {
    let single: BTreeMap<&str, f64> = native_rows
        .iter()
        .filter(|row| row.pinning == "single")
        .map(|row| (row.test.as_str(), row.median_ms))
        .collect();
    let mut ratios: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut implied = Vec::new();
    let mut without_native = 0usize;
    let mut timeout_without_native = 0usize;
    for row in hermit_rows {
        if row.mode != "verify" {
            continue;
        }
        let backend = row.backend.clone().unwrap_or_else(|| "native".into());
        let is_timeout = matches!(
            row.error_kind.as_deref(),
            Some("cpu-timeout" | "wall-timeout")
        );
        if is_timeout {
            match single.get(row.test.as_str()) {
                Some(&native_ms) if native_ms > 0.0 => implied.push(ImpliedSlowdown {
                    test: row.test.clone(),
                    mode: row.mode.clone(),
                    backend: backend.clone(),
                    error_kind: row.error_kind.clone().unwrap_or_default(),
                    budget_seconds: row.timeout_seconds as f64,
                    native_single_median_ms: native_ms,
                    implied_min_slowdown: row.timeout_seconds as f64 / (native_ms / 1000.0),
                }),
                _ => timeout_without_native += 1,
            }
            continue;
        }
        if row.outcome != "PASS" {
            continue;
        }
        let Some(duration_ms) = row.duration_ms else {
            continue;
        };
        match single.get(row.test.as_str()) {
            Some(&native_ms) if native_ms > 0.0 => {
                ratios
                    .entry(backend)
                    .or_default()
                    .push(duration_ms as f64 / native_ms);
            }
            _ => without_native += 1,
        }
    }
    implied.sort_by(|a, b| {
        a.implied_min_slowdown
            .partial_cmp(&b.implied_min_slowdown)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let backends = ratios
        .into_iter()
        .map(|(backend, values)| BackendStats {
            cells: values.len(),
            mean: values.iter().sum::<f64>() / values.len() as f64,
            median: median(&values),
            p90: percentile(&values, 90.0),
            backend,
        })
        .collect();
    StatsReport {
        schema: STATS_SCHEMA.into(),
        baseline: "single-core native median (native-baseline/v1, pinning=single)".into(),
        backends,
        implied,
        hermit_rows_without_native: without_native,
        timeout_rows_without_native: timeout_without_native,
    }
}

fn cmd_stats(args: &[String]) -> ExitCode {
    let mut native_path: Option<PathBuf> = None;
    let mut hermit_paths: Vec<PathBuf> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            "--native" | "--hermit" | "--out" => {
                let flag = args[i].clone();
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error(&format!("{flag} requires a value; see --help"));
                };
                match flag.as_str() {
                    "--native" => native_path = Some(PathBuf::from(value)),
                    "--hermit" => hermit_paths.push(PathBuf::from(value)),
                    _ => out = Some(PathBuf::from(value)),
                }
            }
            other => return usage_error(&format!("unknown stats option {other:?}; see --help")),
        }
        i += 1;
    }
    let Some(native_path) = native_path else {
        return usage_error("--native PATH is required for stats; see --help");
    };
    if hermit_paths.is_empty() {
        return usage_error("--hermit PATH is required for stats (may repeat); see --help");
    }
    let native_rows: Vec<NativeRow> = match read_jsonl(&native_path, "native-baseline") {
        Ok(rows) => rows,
        Err(error) => return fail(&error),
    };
    let mut hermit_rows: Vec<CellResult> = Vec::new();
    for path in &hermit_paths {
        match read_jsonl(path, "CellResult") {
            Ok(mut rows) => hermit_rows.append(&mut rows),
            Err(error) => return fail(&error),
        }
    }
    let report = compute_stats(&native_rows, &hermit_rows);
    println!("backend  cells  mean  median  p90  (Hermit verify ms / single-core native ms)");
    for backend in &report.backends {
        println!(
            "{}  {}  {:.2}  {:.2}  {:.2}",
            backend.backend, backend.cells, backend.mean, backend.median, backend.p90
        );
    }
    println!(
        "implied minimum slowdown for {} timeout cell(s) ({} timeout row(s) without a native row; {} completed row(s) without a native row):",
        report.implied.len(),
        report.timeout_rows_without_native,
        report.hermit_rows_without_native
    );
    for row in &report.implied {
        println!(
            "{} {}@{} {}: budget {:.0}s / native {:.1} ms = {:.1}x minimum",
            row.test,
            row.mode,
            row.backend,
            row.error_kind,
            row.budget_seconds,
            row.native_single_median_ms,
            row.implied_min_slowdown
        );
    }
    if let Some(path) = &out {
        let json = serde_json::to_string_pretty(&report).unwrap_or_default();
        if std::fs::write(path, format!("{json}\n")).is_err() {
            return fail(&format!("cannot write --out {}", path.display()));
        }
    }
    ExitCode::SUCCESS
}

fn cmd_self_test(args: &[String]) -> ExitCode {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if let Some(other) = args.first() {
        return usage_error(&format!(
            "unknown self-test option {other:?}; self-test takes no options"
        ));
    }
    let mut failures = 0usize;
    let mut check = |name: &str, condition: bool| {
        if condition {
            println!("self-test: {name}: ok");
        } else {
            eprintln!("self-test: {name}: FAILED");
            failures += 1;
        }
    };
    check(
        "run-state env wins",
        resolve_run_state(true, Some(Path::new("/x")), Path::new("/w"), true).is_none(),
    );
    check(
        "run-state explicit",
        resolve_run_state(false, Some(Path::new("/x")), Path::new("/w"), false)
            == Some(PathBuf::from("/x")),
    );
    check(
        "run-state prepared default",
        resolve_run_state(true, None, Path::new("/w"), false)
            == Some(PathBuf::from("/w/run-state")),
    );
    check(
        "run-state untouched",
        resolve_run_state(false, None, Path::new("/w"), false).is_none(),
    );
    check("median odd", median(&[3.0, 1.0, 2.0]) == 2.0);
    check("median even", median(&[1.0, 2.0, 3.0, 4.0]) == 2.5);
    check(
        "p90 nearest-rank",
        percentile(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0], 90.0) == 9.0,
    );
    let native = vec![NativeRow {
        schema: ROW_SCHEMA.into(),
        test: "c-programs/example".into(),
        category: "c-programs".into(),
        lane: "portable".into(),
        pinning: "single".into(),
        pin_mechanism: "taskset".into(),
        cpu_set: vec![0],
        samples_ms: vec![100.0],
        median_ms: 100.0,
        min_ms: 100.0,
        exit_codes: vec![Some(0)],
        timed_out: false,
        max_threads: 1,
        max_processes: 1,
        multi_threaded: false,
        multi_process: false,
    }];
    // Row round-trip through the exact on-disk format.
    let round_trip: Result<NativeRow, _> =
        serde_json::to_string(&native[0]).and_then(|line| serde_json::from_str(&line));
    check(
        "row round-trip",
        round_trip.is_ok_and(|row| row == native[0]),
    );
    let hermit_json = r#"{"schema":4,"run_id":"r","attempt":1,"hermit_sha":"s","source_tree_dirty":false,"test":"c-programs/example","category":"c-programs","lane":"portable","mode":"verify","backend":"ptrace","classification":"required","outcome":"PASS","timeout_seconds":15,"duration_ms":1000,"argv":[],"guest_argv":[],"env":{},"cwd":"/","shell_command":"x","attempts":[],"reason":null,"error_kind":null,"artifact_dir":"/r"}"#;
    let passed: CellResult = serde_json::from_str(hermit_json).expect("fixture parses");
    let timed_out: CellResult = serde_json::from_str(
        &hermit_json
            .replace("\"outcome\":\"PASS\"", "\"outcome\":\"FAIL\"")
            .replace("\"error_kind\":null", "\"error_kind\":\"wall-timeout\"")
            .replace("\"duration_ms\":1000", "\"duration_ms\":15000"),
    )
    .expect("fixture parses");
    let report = compute_stats(&native, &[passed, timed_out]);
    check(
        "slowdown median",
        report.backends.len() == 1 && report.backends[0].median == 10.0,
    );
    check(
        "implied slowdown",
        report.implied.len() == 1 && report.implied[0].implied_min_slowdown == 150.0,
    );
    if failures > 0 {
        eprintln!("native-baseline self-test: {failures} check(s) failed");
        return ExitCode::from(1);
    }
    println!("native-baseline self-test: all checks passed");
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("stats") => cmd_stats(&args[1..]),
        Some("self-test") => cmd_self_test(&args[1..]),
        Some("-h" | "--help") => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Some(other) => usage_error(&format!("unknown command {other:?}")),
        None => usage_error("a command is required"),
    }
}
