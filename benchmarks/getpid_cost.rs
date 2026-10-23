#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Measure what one system call costs natively and under each Hermit backend.
//!
//! The guest (`fixtures/getpid_loop.c`) makes N raw `getpid` calls and exits.
//! Hermit virtualizes the guest's clocks, so this harness times each whole run
//! from outside. For every variant it takes the median wall time at several N
//! and fits a least-squares line through the medians at positive N: the slope
//! is the cost of one call with process start-up removed, and the intercept is
//! the fixed cost of a run. A run with N = 0 never reaches the guest's
//! `getpid` site, so it also skips that site's one-time cost (LiteInst traps
//! and patches the site at its first call); its median is reported but left
//! out of the fit. The native fit is the baseline.
//!
//! After timing, one extra run per backend sets
//! `RUST_LOG=hermit::backend_stats=debug` and keeps the backend's own
//! statistics record, which says which path the calls took. LiteInst gets a
//! second such run with zero calls, whose hooks are subtracted. Each of
//! these runs must succeed and print exactly one record for its backend, or
//! the backend gets no fit and the harness exits 1. Timed runs inherit no
//! logging variables, so they log nothing beyond `--log=error`.
//!
//! Run `./benchmarks/getpid_cost.rs --help` for the flags.

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::fs;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const BACKENDS: [&str; 2] = ["ptrace", "liteinst"];
const HERMIT_RUN_FLAGS: [&str; 6] = [
    "--log=error",
    "run",
    "--base-env=minimal",
    "--env=LC_ALL=C",
    "--no-virtualize-cpuid",
    "--max-timeslice=disabled",
];
const STATS_FILTER: &str = "hermit::backend_stats=debug";
const STATS_MARKER: &str = "backend run complete";
/// Removed from every run: an inherited `RUST_LOG` would make timed runs log,
/// and an inherited `HERMIT_LOG_FILE` would move the statistics record off
/// stderr. Only the statistics run sets `RUST_LOG`, to [`STATS_FILTER`].
const LOG_ENV_REMOVED: [&str; 3] = ["RUST_LOG", "HERMIT_LOG", "HERMIT_LOG_FILE"];
/// How often the harness checks whether a run has exited, so each wall time
/// can exceed the run by up to this much plus the host's timer slack.
const POLL_INTERVAL: Duration = Duration::from_micros(500);
const SIGKILL: i32 = 9;

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

const USAGE: &str = "\
Usage: ./benchmarks/getpid_cost.rs [OPTIONS]

Measure the wall-clock cost of one raw getpid call natively and under each
selected Hermit backend. The cost is the least-squares slope of the median
run time across the positive call counts, so process start-up is excluded.
A count of 0 never reaches the fixture's getpid site and so also skips the
site's one-time cost (LiteInst patches the site at its first call): its
median is reported, but it is left out of the fit.

Options:
  --hermit CMD        Hermit command prefix, split on whitespace, so a wrapper
                      may come first; relative paths resolve from the current
                      directory (default: <repository>/target/release/hermit)
  --backends LIST     comma-separated subset of ptrace,liteinst
                      (default: ptrace,liteinst)
  --counts LIST       comma-separated call counts with no duplicates and at
                      least two positive values (default: 0,25000,50000,100000)
  --iterations N      measured runs per variant and count (default: 5)
  --warmups N         unmeasured runs per variant and count (default: 1)
  --timeout SECONDS   kill a run after this many seconds (default: 120)
  --cc CC             C compiler for the fixture (default: cc)
  --output PATH       JSON result file
                      (default: benchmarks/results/getpid-cost.json)
  -h, --help          print this help

Each Hermit run is:
  <hermit> --backend <backend> --log=error run --base-env=minimal \\
    --env=LC_ALL=C --no-virtualize-cpuid --max-timeslice=disabled -- \\
    <fixture> <count>

A sample counts only if it exits 0 and prints exactly \"calls=<count>\".
Failed and timed-out samples stay in the JSON; a variant with any failed
sample gets no fit, and the harness then exits 1.

Every run starts without RUST_LOG, HERMIT_LOG and HERMIT_LOG_FILE. After
timing, one run per backend at the largest count sets
RUST_LOG=hermit::backend_stats=debug, and LiteInst gets the same run with
zero calls first. Each must pass the same check and print exactly one
\"backend run complete backend=<backend> stats=\" record on stderr. For
LiteInst, the record's direct_hook count less the zero-call run's must be at
least the largest count minus 1, which shows that the fixture's getpid site
was patched. Otherwise that backend gets no fit and the harness exits 1.

Wall times come from polling each run every 500 microseconds, so a sample
can read up to one poll interval long.

LiteInst needs libdetcore_liteinst.so beside the Hermit binary, or the
HERMIT_LITEINST_TOOL_RUNTIME environment variable naming it (read by Hermit).
";

struct Options {
    hermit: Option<Vec<String>>,
    backends: Vec<String>,
    counts: Vec<u64>,
    iterations: usize,
    warmups: usize,
    timeout: Duration,
    cc: String,
    output: Option<PathBuf>,
}

fn usage_error(message: &str) -> String {
    format!("{message}\nRun ./benchmarks/getpid_cost.rs --help for the accepted flags.")
}

fn parse_list<T>(
    raw: &str,
    flag: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>, String> {
    let mut values = Vec::new();
    for item in raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        values.push(
            parse(item).ok_or_else(|| usage_error(&format!("{flag}: invalid value {item:?}")))?,
        );
    }
    if values.is_empty() {
        return Err(usage_error(&format!("{flag} needs at least one value")));
    }
    Ok(values)
}

/// Parses the flags after the program name; `Ok(None)` means print the help.
fn parse_options(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, String> {
    let mut options = Options {
        hermit: None,
        backends: BACKENDS.iter().map(|backend| backend.to_string()).collect(),
        counts: vec![0, 25_000, 50_000, 100_000],
        iterations: 5,
        warmups: 1,
        timeout: Duration::from_secs(120),
        cc: "cc".to_string(),
        output: None,
    };
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        if flag == "-h" || flag == "--help" {
            return Ok(None);
        }
        let mut value = || {
            args.next()
                .ok_or_else(|| usage_error(&format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--hermit" => {
                let words: Vec<String> = value()?.split_whitespace().map(str::to_string).collect();
                if words.is_empty() {
                    return Err(usage_error("--hermit needs a command"));
                }
                options.hermit = Some(words);
            }
            "--backends" => {
                options.backends = parse_list(&value()?, "--backends", |item| {
                    BACKENDS.contains(&item).then(|| item.to_string())
                })?;
            }
            "--counts" => {
                options.counts = parse_list(&value()?, "--counts", |item| item.parse().ok())?;
            }
            "--iterations" => {
                options.iterations = value()?
                    .parse()
                    .ok()
                    .filter(|iterations| *iterations >= 1)
                    .ok_or_else(|| {
                        usage_error("--iterations must be a whole number of at least 1")
                    })?;
            }
            "--warmups" => {
                options.warmups = value()?
                    .parse()
                    .map_err(|_| usage_error("--warmups must be a whole number"))?;
            }
            "--timeout" => {
                // try_from refuses NaN, infinities, negatives and values too large
                // for a Duration; a positive value that rounds to zero is refused
                // here, since a zero bound would time out every run.
                options.timeout = value()?
                    .parse::<f64>()
                    .ok()
                    .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
                    .filter(|timeout| !timeout.is_zero())
                    .ok_or_else(|| usage_error("--timeout must be a positive number of seconds"))?;
            }
            "--cc" => options.cc = value()?,
            "--output" => options.output = Some(PathBuf::from(value()?)),
            _ => return Err(usage_error(&format!("unknown flag {flag:?}"))),
        }
    }
    let mut distinct = options.counts.clone();
    distinct.sort_unstable();
    distinct.dedup();
    // The fit uses positive counts only (see `fitted_points`), so a line
    // needs two of them. Two distinct positive counts also make the largest
    // at least 2, which LiteInst's statistics check needs.
    if distinct.len() != options.counts.len() || distinct.iter().filter(|&&c| c > 0).count() < 2 {
        return Err(usage_error(
            "--counts needs at least two positive values and no duplicates",
        ));
    }
    let mut backends = options.backends.clone();
    backends.sort();
    backends.dedup();
    if backends.len() != options.backends.len() {
        return Err(usage_error("--backends contains a duplicate"));
    }
    if options.warmups.checked_add(options.iterations).is_none() {
        return Err(usage_error("--warmups plus --iterations is too large"));
    }
    Ok(Some(options))
}

fn capture(program: &str, args: &[&str], root: &Path) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn repository_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    capture("git", &["rev-parse", "--show-toplevel"], &cwd)
        .map(PathBuf::from)
        .map_err(|error| format!("{error}\nRun this harness from inside the Hermit checkout."))
}

enum Outcome {
    Ok,
    Failed(String),
    TimedOut,
}

struct Sample {
    variant: String,
    calls: u64,
    round: usize,
    warmup: bool,
    wall_ns: u128,
    outcome: Outcome,
}

/// Runs `argv` to completion with its output in files, so a large stderr can
/// never block the child on a full pipe while the parent polls.
fn run_bounded(
    argv: &[String],
    env: &[(&str, &str)],
    timeout: Duration,
    scratch: &Path,
) -> Result<(Duration, Option<std::process::ExitStatus>, String, String), String> {
    let stdout_path = scratch.join("stdout");
    let stderr_path = scratch.join("stderr");
    let stdout = File::create(&stdout_path)
        .map_err(|error| format!("cannot create {}: {error}", stdout_path.display()))?;
    let stderr = File::create(&stderr_path)
        .map_err(|error| format!("cannot create {}: {error}", stderr_path.display()))?;
    let mut command = Command::new(&argv[0]);
    for name in LOG_ENV_REMOVED {
        command.env_remove(name);
    }
    command
        .args(&argv[1..])
        .envs(env.iter().copied())
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        // Its own process group, so a timeout also kills anything a wrapper started.
        .process_group(0);
    let start = Instant::now();
    let mut child = command.spawn().map_err(|error| {
        format!(
            "cannot start {}: {error}\nCheck --hermit and --cc.",
            argv[0]
        )
    })?;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot wait for {}: {error}", argv[0]))?
        {
            break Some(status);
        }
        if start.elapsed() >= timeout {
            // SAFETY: the group leader has not been reaped, so its id still names this group.
            unsafe { kill(-(child.id() as i32), SIGKILL) };
            child
                .wait()
                .map_err(|error| format!("cannot reap {}: {error}", argv[0]))?;
            break None;
        }
        thread::sleep(POLL_INTERVAL);
    };
    let elapsed = start.elapsed();
    let read =
        |path: &Path| fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let stdout = read(&stdout_path)
        .map_err(|error| format!("cannot read {}: {error}", stdout_path.display()))?;
    let stderr = read(&stderr_path)
        .map_err(|error| format!("cannot read {}: {error}", stderr_path.display()))?;
    Ok((elapsed, status, stdout, stderr))
}

fn classify(
    status: Option<std::process::ExitStatus>,
    stdout: &str,
    stderr: &str,
    calls: u64,
) -> Outcome {
    let Some(status) = status else {
        return Outcome::TimedOut;
    };
    let tail = stderr.lines().next_back().unwrap_or("").to_string();
    match (status.code(), status.signal()) {
        (Some(0), _) if stdout == format!("calls={calls}\n") => Outcome::Ok,
        (Some(0), _) => Outcome::Failed(format!("unexpected stdout {stdout:?}")),
        (Some(code), _) => Outcome::Failed(format!("exit {code}: {tail}")),
        (None, Some(signal)) => Outcome::Failed(format!("signal {signal}: {tail}")),
        (None, None) => Outcome::Failed(format!("unknown status: {tail}")),
    }
}

/// The lines of `stderr` that carry a backend statistics record.
fn stats_records(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| line.contains(STATS_MARKER))
        .map(str::to_string)
        .collect()
}

/// The `name=count` pairs inside a LiteInst record's `paths[...]` list, in
/// record order. `None` if the list is absent or any pair does not parse.
fn dispatch_paths(record: &str) -> Option<Vec<(String, u64)>> {
    let start = record.find(" paths[")? + " paths[".len();
    let end = start + record[start..].find(']')?;
    record[start..end]
        .split(',')
        .map(|pair| {
            let (name, count) = pair.split_once('=')?;
            Some((name.to_string(), count.parse().ok()?))
        })
        .collect()
}

/// A statistics run counts only if the guest ran correctly and Hermit printed
/// exactly one record, for this backend. Anything else means the counters
/// describe another run, or nothing at all.
///
/// For LiteInst the record must also carry a readable `direct_hook` count,
/// which is returned for [`check_hooked_calls`]; other backends return `None`.
fn check_stats_run(
    backend: &str,
    outcome: &Outcome,
    records: &[String],
) -> Result<Option<u64>, String> {
    match outcome {
        Outcome::Ok => {}
        Outcome::Failed(reason) => return Err(format!("the run failed: {reason}")),
        Outcome::TimedOut => return Err("the run timed out".to_string()),
    }
    let own = format!("{STATS_MARKER} backend={backend} stats=");
    let record = match records {
        [record] if record.contains(&own) => record,
        [record] => return Err(format!("its only record is not {own:?}: {record:?}")),
        _ => {
            return Err(format!(
                "expected exactly one {STATS_MARKER:?} record, found {}",
                records.len()
            ));
        }
    };
    if backend != "liteinst" {
        return Ok(None);
    }
    let paths = dispatch_paths(record)
        .ok_or_else(|| format!("its record has no readable paths[...] list: {record:?}"))?;
    paths
        .iter()
        .find(|(name, _)| name == "direct_hook")
        .map(|(_, count)| Some(*count))
        .ok_or_else(|| format!("its paths[...] list has no direct_hook counter: {record:?}"))
}

/// The fixture's own `getpid` site must have been patched. `direct` counts
/// hooks at every patched site, so the hooks of a zero-call run (`baseline`,
/// start-up and exit sites only) are subtracted first. What remains must be at
/// least `calls - 1`, the one allowance being the first call, which may arrive
/// through a trap before the site is patched. Otherwise every call trapped or
/// went uncounted, and the fitted slope is the cost of a trap, not of a hooked
/// call. A run of fewer than two calls owes nothing to the hook, so it cannot
/// show a hooked call and is refused too.
fn check_hooked_calls(calls: u64, direct: u64, baseline: u64) -> Result<(), String> {
    let floor = calls.saturating_sub(1);
    if floor == 0 {
        return Err(format!(
            "calls={calls} owes no call to direct_hook: a LiteInst statistics run needs at least 2 calls to show one through the patched site"
        ));
    }
    let hooked = direct.saturating_sub(baseline);
    if hooked < floor {
        return Err(format!(
            "direct_hook={direct} less the zero-call run's {baseline} is {hooked}, below {floor}: the getpid site was not patched, so the fitted slope is not the cost of a hooked call"
        ));
    }
    Ok(())
}

/// The call counts of one backend's statistics runs, in order. LiteInst first
/// runs the fixture with zero calls, so that hooks taken at start-up and exit
/// sites can be subtracted from the measured run's; every backend then runs
/// at the largest count.
fn stats_run_counts(backend: &str, largest: u64) -> Vec<u64> {
    let baseline_run = (backend == "liteinst").then_some(0);
    baseline_run.into_iter().chain([largest]).collect()
}

/// The verdict on one statistics run, in [`stats_run_counts`] order. LiteInst's
/// zero-call run stores its `direct_hook` count in `baseline`; the measured run
/// is then checked against it by [`check_hooked_calls`], and is refused if the
/// zero-call run was rejected.
fn stats_verdict(
    backend: &str,
    calls: u64,
    outcome: &Outcome,
    records: &[String],
    baseline: &mut Option<u64>,
) -> Result<(), String> {
    match check_stats_run(backend, outcome, records)? {
        None => Ok(()),
        Some(direct) if calls == 0 => {
            *baseline = Some(direct);
            Ok(())
        }
        Some(direct) => match *baseline {
            Some(baseline) => check_hooked_calls(calls, direct, baseline),
            None => Err(
                "the zero-call run was rejected, so the getpid site's hooks cannot be told from start-up and exit hooks".to_string(),
            ),
        },
    }
}

/// One statistics run: backend, call count, outcome, records and verdict.
type StatsRun = (String, u64, String, Vec<String>, Result<(), String>);

/// Whether every statistics run of `variant` was accepted, judged on that
/// backend's own runs only; `None` for native, which has none.
fn stats_accepted(variant: &str, stats_runs: &[StatsRun]) -> Option<bool> {
    (variant != "native").then(|| {
        stats_runs
            .iter()
            .filter(|(backend, ..)| backend == variant)
            .all(|(.., verdict)| verdict.is_ok())
    })
}

fn variant_argv(hermit: &[String], variant: &str, fixture: &Path, calls: u64) -> Vec<String> {
    let mut argv = Vec::new();
    if variant != "native" {
        argv.extend(hermit.iter().cloned());
        argv.push(format!("--backend={variant}"));
        argv.extend(HERMIT_RUN_FLAGS.iter().map(|flag| flag.to_string()));
        argv.push("--".to_string());
    }
    argv.push(fixture.display().to_string());
    argv.push(calls.to_string());
    argv
}

fn median(sorted: &[f64]) -> f64 {
    let middle = sorted.len() / 2;
    if !sorted.len().is_multiple_of(2) {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    }
}

/// Median and median absolute deviation, in the samples' unit.
fn median_and_mad(values: &[f64]) -> (f64, f64) {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let center = median(&sorted);
    let mut deviations: Vec<f64> = sorted.iter().map(|value| (value - center).abs()).collect();
    deviations.sort_by(f64::total_cmp);
    (center, median(&deviations))
}

/// Ordinary least squares through `(x, y)` points: `(slope, intercept)`, or
/// `None` when the line is undefined (fewer than two distinct x values) or
/// not finite.
fn fit_line(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / n;
    let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / n;
    let covariance: f64 = points
        .iter()
        .map(|(x, y)| (x - mean_x) * (y - mean_y))
        .sum();
    let variance: f64 = points.iter().map(|(x, _)| (x - mean_x).powi(2)).sum();
    let slope = covariance / variance;
    let intercept = mean_y - slope * mean_x;
    (slope.is_finite() && intercept.is_finite()).then_some((slope, intercept))
}

/// The `(count, median)` points the fit uses: positive counts only. A run
/// with zero calls never reaches the fixture's `getpid` site, so it skips the
/// site's one-time cost that every positive count pays (LiteInst's first
/// trap and patch installation). Each sample is a fresh process, so warmups
/// cannot remove that step, and fitting it would add it to the slope: a
/// noiseless `T(0) = F`, `T(N > 0) = F + D + cN` fitted at counts 0 and 2
/// gives `c + D/2`.
fn fitted_points(points: &[(u64, f64, f64, usize)]) -> Vec<(f64, f64)> {
    points
        .iter()
        .filter(|(calls, ..)| *calls > 0)
        .map(|(calls, center, _, _)| (*calls as f64, *center))
        .collect()
}

/// One variant's result: medians come from measured runs only, but a failed
/// warmup or a rejected statistics run refuses the fit exactly as a failed
/// measured run does.
struct Summary {
    variant: String,
    measured: usize,
    failed: usize,
    failed_warmups: usize,
    /// Whether every statistics run of this backend was accepted; `None` for
    /// `native`, which has none.
    stats_accepted: Option<bool>,
    /// `(calls, median ns, MAD ns, runs)` for each count with a successful
    /// run, including a zero count that the fit leaves out.
    points: Vec<(u64, f64, f64, usize)>,
    /// `(ns per call, intercept ns)` over the positive counts (see
    /// [`fitted_points`]), or `None` after any failure.
    fit: Option<(f64, f64)>,
}

fn summarize(
    samples: &[Sample],
    variant: &str,
    counts: &[u64],
    stats_accepted: Option<bool>,
) -> Summary {
    let failures = |warmup: bool| {
        samples
            .iter()
            .filter(|s| s.variant == variant && s.warmup == warmup)
            .filter(|s| !matches!(s.outcome, Outcome::Ok))
            .count()
    };
    let measured: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.variant == variant && !s.warmup)
        .collect();
    let (failed, failed_warmups) = (failures(false), failures(true));
    let mut points = Vec::new();
    for &calls in counts {
        let walls: Vec<f64> = measured
            .iter()
            .filter(|s| s.calls == calls && matches!(s.outcome, Outcome::Ok))
            .map(|s| s.wall_ns as f64)
            .collect();
        if !walls.is_empty() {
            let (center, mad) = median_and_mad(&walls);
            points.push((calls, center, mad, walls.len()));
        }
    }
    let fit = (failed == 0 && failed_warmups == 0 && stats_accepted != Some(false))
        .then(|| fit_line(&fitted_points(&points)))
        .flatten();
    Summary {
        variant: variant.to_string(),
        measured: measured.len(),
        failed,
        failed_warmups,
        stats_accepted,
        points,
        fit,
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32))
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn json_list(items: impl IntoIterator<Item = String>) -> String {
    format!("[{}]", items.into_iter().collect::<Vec<_>>().join(", "))
}

fn short_hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().split('.').next().unwrap_or("").to_string())
        .unwrap_or_default()
}

fn first_line_with(path: &str, prefix: &str) -> String {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with(prefix))
                .and_then(|line| line.split_once(':'))
                .map(|(_, value)| value.trim().to_string())
        })
        .unwrap_or_default()
}

fn sha256_of(path: &Path, root: &Path) -> String {
    capture("sha256sum", &[&path.display().to_string()], root)
        .ok()
        .and_then(|line| line.split_whitespace().next().map(str::to_string))
        .unwrap_or_default()
}

fn run(options: &Options) -> Result<bool, String> {
    let root = repository_root()?;
    let work = root
        .join("target")
        .join("hermit-benchmarks")
        .join("getpid-cost");
    fs::create_dir_all(&work)
        .map_err(|error| format!("cannot create {}: {error}", work.display()))?;
    let source = root
        .join("benchmarks")
        .join("fixtures")
        .join("getpid_loop.c");
    let fixture = work.join("getpid_loop");
    let source_arg = source.display().to_string();
    let fixture_arg = fixture.display().to_string();
    capture(
        &options.cc,
        &[
            "-O2",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            &source_arg,
            "-o",
            &fixture_arg,
        ],
        &root,
    )
    .map_err(|error| format!("{error}\nInstall a C11 compiler or pass --cc."))?;

    let hermit = options.hermit.clone().unwrap_or_else(|| {
        vec![
            root.join("target")
                .join("release")
                .join("hermit")
                .display()
                .to_string(),
        ]
    });
    let mut variants = vec!["native".to_string()];
    variants.extend(options.backends.iter().cloned());
    let started_utc = capture("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"], &root)?;
    let load_before = fs::read_to_string("/proc/loadavg").unwrap_or_default();

    // Rotate the variant order every round so no variant always runs first.
    let mut samples = Vec::new();
    for round in 0..options.warmups + options.iterations {
        let warmup = round < options.warmups;
        for &calls in &options.counts {
            for offset in 0..variants.len() {
                let variant = &variants[(round + offset) % variants.len()];
                let argv = variant_argv(&hermit, variant, &fixture, calls);
                let (elapsed, status, stdout, stderr) =
                    run_bounded(&argv, &[], options.timeout, &work)?;
                let outcome = classify(status, &stdout, &stderr, calls);
                if let Outcome::Failed(reason) = &outcome {
                    eprintln!("FAILED {variant} calls={calls} round={round}: {reason}");
                } else if let Outcome::TimedOut = outcome {
                    eprintln!("TIMED OUT {variant} calls={calls} round={round}");
                }
                samples.push(Sample {
                    variant: variant.clone(),
                    calls,
                    round,
                    warmup,
                    wall_ns: elapsed.as_nanos(),
                    outcome,
                });
            }
        }
    }

    // One unmeasured run per backend that asks the backend for its own counters,
    // after LiteInst's zero-call run (see stats_run_counts).
    let stats_calls = *options.counts.iter().max().expect("at least two counts");
    let mut stats_runs: Vec<StatsRun> = Vec::new();
    for backend in &options.backends {
        let mut baseline = None;
        for calls in stats_run_counts(backend, stats_calls) {
            let argv = variant_argv(&hermit, backend, &fixture, calls);
            let (_, status, stdout, stderr) =
                run_bounded(&argv, &[("RUST_LOG", STATS_FILTER)], options.timeout, &work)?;
            let records = stats_records(&stderr);
            let classified = classify(status, &stdout, &stderr, calls);
            let verdict = stats_verdict(backend, calls, &classified, &records, &mut baseline);
            if let Err(reason) = &verdict {
                eprintln!("FAILED stats run {backend} calls={calls}: {reason}");
            }
            let outcome = match classified {
                Outcome::Ok => "ok".to_string(),
                Outcome::Failed(reason) => format!("failed: {reason}"),
                Outcome::TimedOut => "timed out".to_string(),
            };
            stats_runs.push((backend.clone(), calls, outcome, records, verdict));
        }
    }
    let load_after = fs::read_to_string("/proc/loadavg").unwrap_or_default();

    // Summaries are computed from the raw rows above and nothing else.
    let mut all_ok = stats_runs.iter().all(|(.., verdict)| verdict.is_ok());
    let summaries: Vec<Summary> = variants
        .iter()
        .map(|variant| {
            let accepted = stats_accepted(variant, &stats_runs);
            summarize(&samples, variant, &options.counts, accepted)
        })
        .collect();
    all_ok &= summaries.iter().all(|summary| summary.fit.is_some());
    let native_slope = summaries
        .iter()
        .find(|summary| summary.variant == "native")
        .and_then(|summary| summary.fit.map(|(slope, _)| slope));

    println!(
        "{} measured runs per point; median wall time at each call count; fit = least squares over the medians",
        options.iterations
    );
    println!(
        "{:<10} {:>12} {:>16} {:>12}  per-count median ms (MAD ms)",
        "variant", "us/call", "extra us/call", "fixed ms"
    );
    for Summary {
        variant,
        failed,
        failed_warmups,
        stats_accepted,
        points,
        fit,
        ..
    } in &summaries
    {
        let per_count = points
            .iter()
            .map(|(calls, center, mad, n)| {
                format!("{calls}: {:.3} ({:.3}, n={n})", center / 1e6, mad / 1e6)
            })
            .collect::<Vec<_>>()
            .join("; ");
        match fit {
            Some((slope, intercept)) => {
                let extra = native_slope
                    .filter(|_| variant != "native")
                    .map(|native| format!("{:.3}", (slope - native) / 1e3))
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{variant:<10} {:>12.3} {extra:>16} {:>12.2}  {per_count}",
                    slope / 1e3,
                    intercept / 1e6
                );
            }
            None => {
                let mut reasons = Vec::new();
                if *stats_accepted == Some(false) {
                    reasons.push("statistics run rejected".to_string());
                }
                if *failed > 0 || *failed_warmups > 0 {
                    reasons.push(format!(
                        "{failed} failed sample(s), {failed_warmups} failed warmup(s)"
                    ));
                }
                let reason = if reasons.is_empty() {
                    "least-squares line undefined".to_string()
                } else {
                    reasons.join(", ")
                };
                println!(
                    "{variant:<10} {:>12} {:>16} {:>12}  {reason}; {per_count}",
                    "no fit", "-", "-"
                );
            }
        }
    }
    for (backend, calls, outcome, records, verdict) in &stats_runs {
        let accepted = match verdict {
            Ok(()) => "accepted".to_string(),
            Err(reason) => format!("REJECTED: {reason}"),
        };
        println!("stats run {backend} calls={calls} ({outcome}; {accepted}):");
        if records.is_empty() {
            println!("  no {STATS_MARKER:?} record on stderr");
        }
        for record in records {
            if let Some(paths) = dispatch_paths(record) {
                let nonzero = paths
                    .iter()
                    .filter(|(_, count)| *count > 0)
                    .map(|(name, count)| format!("{name}={count}"))
                    .collect::<Vec<_>>();
                println!(
                    "  dispatch paths over {calls} getpid calls: {}",
                    if nonzero.is_empty() {
                        "all zero".to_string()
                    } else {
                        nonzero.join(" ")
                    }
                );
            }
            println!("  {record}");
        }
    }

    let output = options.output.clone().unwrap_or_else(|| {
        root.join("benchmarks")
            .join("results")
            .join("getpid-cost.json")
    });
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let head = capture("git", &["rev-parse", "HEAD"], &root)?;
    let dirty = !capture(
        "git",
        &["status", "--porcelain", "--untracked-files=no"],
        &root,
    )?
    .is_empty();
    let script = root.join("benchmarks").join("getpid_cost.rs");
    // Relative prefix words resolve against the caller's directory, as they do when spawned.
    let prefix_digests = json_list(hermit.iter().filter(|word| Path::new(word).is_file()).map(
        |word| {
            format!(
                "{{\"path\": {}, \"sha256\": {}}}",
                json_string(word),
                json_string(
                    &std::path::absolute(word)
                        .map(|path| sha256_of(&path, &root))
                        .unwrap_or_default()
                )
            )
        },
    ));
    let summary_json = json_list(summaries.iter().map(|Summary { variant, measured, failed, failed_warmups, stats_accepted, points, fit }| {
        let points_json = json_list(points.iter().map(|(calls, center, mad, n)| {
            format!("{{\"calls\": {calls}, \"median_ns\": {center}, \"mad_ns\": {mad}, \"samples\": {n}}}")
        }));
        let fit_json = match fit {
            Some((slope, intercept)) => format!("{{\"ns_per_call\": {slope}, \"intercept_ns\": {intercept}}}"),
            None => "null".to_string(),
        };
        let stats_accepted_json = match stats_accepted {
            Some(accepted) => accepted.to_string(),
            None => "null".to_string(),
        };
        format!(
            "{{\"variant\": {}, \"measured_samples\": {measured}, \"failed_samples\": {failed}, \"failed_warmup_samples\": {failed_warmups}, \"stats_accepted\": {stats_accepted_json}, \"points\": {points_json}, \"fit\": {fit_json}}}",
            json_string(variant)
        )
    }));
    let samples_json = json_list(samples.iter().map(|sample| {
        let (status, reason) = match &sample.outcome {
            Outcome::Ok => ("ok", String::new()),
            Outcome::Failed(reason) => ("failed", reason.clone()),
            Outcome::TimedOut => ("timed_out", String::new()),
        };
        format!(
            "{{\"variant\": {}, \"calls\": {}, \"round\": {}, \"warmup\": {}, \"wall_ns\": {}, \"status\": {}, \"reason\": {}}}",
            json_string(&sample.variant),
            sample.calls,
            sample.round,
            sample.warmup,
            sample.wall_ns,
            json_string(status),
            json_string(&reason)
        )
    }));
    let stats_json = json_list(stats_runs.iter().map(|(backend, calls, outcome, records, verdict)| {
        format!(
            "{{\"backend\": {}, \"calls\": {calls}, \"rust_log\": {}, \"outcome\": {}, \"accepted\": {}, \"rejection\": {}, \"records\": {}, \"dispatch_paths\": {}}}",
            json_string(backend),
            json_string(STATS_FILTER),
            json_string(outcome),
            verdict.is_ok(),
            json_string(verdict.as_ref().err().map(String::as_str).unwrap_or("")),
            json_list(records.iter().map(|record| json_string(record))),
            json_list(records.iter().map(|record| match dispatch_paths(record) {
                Some(paths) => format!(
                    "{{{}}}",
                    paths
                        .iter()
                        .map(|(name, count)| format!("{}: {count}", json_string(name)))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                None => "null".to_string(),
            }))
        )
    }));
    let example_argv = |variant: &str| {
        json_list(
            variant_argv(&hermit, variant, &fixture, 0)
                .iter()
                .map(|word| json_string(word)),
        )
    };
    let json = format!(
        "{{\n  \"schema\": \"hermit-getpid-cost/1\",\n  \"started_utc\": {},\n  \"host\": {},\n  \"kernel\": {},\n  \"cpu_model\": {},\n  \"load_before\": {},\n  \"load_after\": {},\n  \"cgroup_isolation\": \"none: this harness does not create a cgroup; record ambient load\",\n  \"repository_head\": {},\n  \"repository_dirty\": {},\n  \"producer_sha256\": {},\n  \"fixture_sha256\": {},\n  \"hermit_prefix\": {},\n  \"hermit_prefix_digests\": {},\n  \"command_shapes\": {{\"native\": {}, \"hermit\": {}}},\n  \"counts\": {},\n  \"warmups\": {},\n  \"iterations\": {},\n  \"timeout_seconds\": {},\n  \"order\": \"each round runs every count; within a count the variant order rotates by round\",\n  \"statistic\": \"median wall ns per (variant, count) with median absolute deviation; fit = ordinary least squares of median against count over the positive counts; a zero count is reported but not fitted\",\n  \"poll_interval_ns\": {},\n  \"wall_clock_resolution\": \"each run is polled every poll_interval_ns, so a wall time can exceed the run by up to one poll interval plus the host's timer slack\",\n  \"log_environment\": {{\"removed_from_every_run\": {}, \"statistics_run_rust_log\": {}}},\n  \"summaries\": {},\n  \"stats_runs\": {},\n  \"samples\": {}\n}}\n",
        json_string(&started_utc),
        json_string(&short_hostname()),
        json_string(
            fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .trim()
        ),
        json_string(&first_line_with("/proc/cpuinfo", "model name")),
        json_string(load_before.trim()),
        json_string(load_after.trim()),
        json_string(&head),
        dirty,
        json_string(&sha256_of(&script, &root)),
        json_string(&sha256_of(&source, &root)),
        json_list(hermit.iter().map(|word| json_string(word))),
        prefix_digests,
        example_argv("native"),
        example_argv(
            options
                .backends
                .first()
                .map(String::as_str)
                .unwrap_or("ptrace")
        ),
        json_list(options.counts.iter().map(u64::to_string)),
        options.warmups,
        options.iterations,
        options.timeout.as_secs_f64(),
        POLL_INTERVAL.as_nanos(),
        json_list(LOG_ENV_REMOVED.iter().map(|name| json_string(name))),
        json_string(STATS_FILTER),
        summary_json,
        stats_json,
        samples_json,
    );
    fs::write(&output, json)
        .map_err(|error| format!("cannot write {}: {error}", output.display()))?;
    println!("raw rows and metadata: {}", output.display());
    Ok(all_ok)
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let options = match parse_options(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("getpid_cost: {message}");
            return ExitCode::from(2);
        }
    };
    match run(&options) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!(
                "getpid_cost: some samples or statistics runs failed; see the FAILED lines above and the JSON"
            );
            ExitCode::from(1)
        }
        Err(message) => {
            eprintln!("getpid_cost: {message}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    fn exited(code: i32) -> Option<std::process::ExitStatus> {
        Some(std::process::ExitStatus::from_raw(code << 8))
    }

    #[test]
    fn classify_accepts_only_a_clean_exit_with_the_exact_count() {
        assert!(matches!(
            classify(exited(0), "calls=7\n", "", 7),
            Outcome::Ok
        ));
        assert!(matches!(
            classify(exited(0), "calls=6\n", "", 7),
            Outcome::Failed(_)
        ));
        assert!(matches!(
            classify(exited(0), "calls=7", "", 7),
            Outcome::Failed(_)
        ));
        assert!(matches!(
            classify(exited(1), "calls=7\n", "boom\n", 7),
            Outcome::Failed(reason) if reason == "exit 1: boom"
        ));
        assert!(matches!(
            classify(
                Some(std::process::ExitStatus::from_raw(SIGKILL)),
                "",
                "",
                7
            ),
            Outcome::Failed(reason) if reason.starts_with("signal 9")
        ));
        assert!(matches!(
            classify(None, "calls=7\n", "", 7),
            Outcome::TimedOut
        ));
    }

    #[test]
    fn parse_options_rejects_counts_that_cannot_define_a_line() {
        assert!(parse_options(args(&["--counts", "25,100"])).is_ok());
        assert!(parse_options(args(&["--counts", "0,25,100"])).is_ok());
        assert!(parse_options(args(&["--counts", "1,2"])).is_ok());
        assert!(parse_options(args(&["--counts", "100"])).is_err());
        assert!(parse_options(args(&["--counts", "5,5"])).is_err());
        assert!(parse_options(args(&["--counts", "0,100,100"])).is_err());
        // Zero is not fitted, so it does not make the second point of a line.
        assert!(parse_options(args(&["--counts", "0,100"])).is_err());
        assert!(parse_options(args(&["--counts", "0,1"])).is_err());
    }

    #[test]
    fn parse_options_rejects_overflowing_round_counts_and_duplicates() {
        let max = usize::MAX.to_string();
        assert!(parse_options(args(&["--warmups", &max])).is_err());
        assert!(parse_options(args(&["--backends", "ptrace,ptrace"])).is_err());
        assert!(parse_options(args(&["--backends", "kvm"])).is_err());
        assert!(parse_options(args(&["--iterations", "0"])).is_err());
        assert!(parse_options(args(&["--timeout", "0"])).is_err());
        // Too large for a Duration, and positive but below one nanosecond.
        assert!(parse_options(args(&["--timeout", "1e100"])).is_err());
        assert!(parse_options(args(&["--timeout", "1e-12"])).is_err());
        assert!(parse_options(args(&["--timeout", "nan"])).is_err());
        assert!(parse_options(args(&["--timeout", "-1"])).is_err());
        assert!(parse_options(args(&["--bogus"])).is_err());
        assert!(parse_options(args(&["--cc"])).is_err());
    }

    #[test]
    fn parse_options_help_and_defaults() {
        assert!(matches!(parse_options(args(&["--help"])), Ok(None)));
        assert!(matches!(parse_options(args(&["-h"])), Ok(None)));
        let options = parse_options(args(&[])).unwrap().unwrap();
        assert_eq!(options.backends, ["ptrace", "liteinst"]);
        assert_eq!(options.counts, [0, 25_000, 50_000, 100_000]);
        assert_eq!((options.iterations, options.warmups), (5, 1));
    }

    #[test]
    fn fit_line_recovers_an_exact_line_and_refuses_undefined_ones() {
        assert_eq!(fit_line(&[(0.0, 1.0), (10.0, 21.0)]), Some((2.0, 1.0)));
        assert_eq!(
            fit_line(&[(0.0, 1.0), (10.0, 21.0), (20.0, 41.0)]),
            Some((2.0, 1.0))
        );
        assert_eq!(fit_line(&[]), None);
        assert_eq!(fit_line(&[(3.0, 4.0)]), None);
        assert_eq!(fit_line(&[(3.0, 4.0), (3.0, 9.0)]), None);
    }

    #[test]
    fn summarize_refuses_a_fit_after_a_failed_warmup_but_keeps_warmups_out_of_medians() {
        let sample = |calls: u64, warmup: bool, wall_ns: u128, outcome: Outcome| Sample {
            variant: "ptrace".to_string(),
            calls,
            round: 0,
            warmup,
            wall_ns,
            outcome,
        };
        let measured = || {
            vec![
                sample(10, false, 3_000, Outcome::Ok),
                sample(20, false, 5_000, Outcome::Ok),
            ]
        };

        let mut clean = measured();
        // A slow but successful warmup affects neither the medians nor the fit.
        clean.push(sample(10, true, 900_000, Outcome::Ok));
        let summary = summarize(&clean, "ptrace", &[10, 20], Some(true));
        assert_eq!((summary.measured, summary.failed), (2, 0));
        assert_eq!(summary.failed_warmups, 0);
        assert_eq!(summary.points[0].1, 3_000.0);
        assert_eq!(summary.fit, Some((200.0, 1_000.0)));

        for outcome in [Outcome::TimedOut, Outcome::Failed("calls=9".to_string())] {
            let mut warm_failure = measured();
            warm_failure.push(sample(20, true, 5_000, outcome));
            let summary = summarize(&warm_failure, "ptrace", &[10, 20], Some(true));
            assert_eq!((summary.failed, summary.failed_warmups), (0, 1));
            assert_eq!(summary.points.len(), 2);
            assert_eq!(summary.fit, None);
        }

        let mut measured_failure = measured();
        measured_failure.push(sample(20, false, 5_000, Outcome::TimedOut));
        let summary = summarize(&measured_failure, "ptrace", &[10, 20], Some(true));
        assert_eq!((summary.failed, summary.failed_warmups), (1, 0));
        assert_eq!(summary.fit, None);
        // Another variant's failure does not refuse this one.
        for (calls, wall_ns) in [(10, 600), (20, 700)] {
            measured_failure.push(Sample {
                variant: "native".to_string(),
                ..sample(calls, false, wall_ns, Outcome::Ok)
            });
        }
        let native = summarize(&measured_failure, "native", &[10, 20], None);
        assert_eq!((native.failed, native.failed_warmups), (0, 0));
        assert_eq!(native.fit, Some((10.0, 500.0)));
    }

    #[test]
    fn summarize_refuses_a_fit_when_the_statistics_run_was_rejected() {
        let samples: Vec<Sample> = [(10, 3_000), (20, 5_000)]
            .into_iter()
            .map(|(calls, wall_ns)| Sample {
                variant: "liteinst".to_string(),
                calls,
                round: 0,
                warmup: false,
                wall_ns,
                outcome: Outcome::Ok,
            })
            .collect();
        let accepted = summarize(&samples, "liteinst", &[10, 20], Some(true));
        assert_eq!(accepted.fit, Some((200.0, 1_000.0)));
        // Clean timed samples, but the statistics run says the calls did not
        // take the hook: the slope would be a trap's cost, so no fit.
        let rejected = summarize(&samples, "liteinst", &[10, 20], Some(false));
        assert_eq!(rejected.stats_accepted, Some(false));
        assert_eq!((rejected.failed, rejected.failed_warmups), (0, 0));
        assert_eq!(rejected.points.len(), 2);
        assert_eq!(rejected.fit, None);
    }

    #[test]
    fn summarize_reports_a_zero_count_but_leaves_it_out_of_the_fit() {
        // Noiseless runs with a fixed cost F = 1000 ns, a one-time site cost
        // D = 50000 ns that only positive counts pay, and c = 200 ns per call.
        let wall = |calls: u64| {
            if calls == 0 {
                1_000
            } else {
                51_000 + 200 * calls as u128
            }
        };
        let samples = |counts: &[u64]| -> Vec<Sample> {
            counts
                .iter()
                .map(|&calls| Sample {
                    variant: "liteinst".to_string(),
                    calls,
                    round: 0,
                    warmup: false,
                    wall_ns: wall(calls),
                    outcome: Outcome::Ok,
                })
                .collect()
        };
        let summary = summarize(&samples(&[0, 2, 4]), "liteinst", &[0, 2, 4], Some(true));
        // The zero count is still reported...
        assert_eq!(summary.points.len(), 3);
        assert_eq!((summary.points[0].0, summary.points[0].1), (0, 1_000.0));
        // ...but the slope is c, not c + D/2, and the intercept is F + D.
        assert_eq!(summary.fit, Some((200.0, 51_000.0)));
        // With only one positive count there is no line to fit.
        let one = summarize(&samples(&[0, 2]), "liteinst", &[0, 2], Some(true));
        assert_eq!(one.points.len(), 2);
        assert_eq!(one.fit, None);
        assert_eq!(fitted_points(&one.points), [(2.0, 51_400.0)]);
    }

    #[test]
    fn median_and_mad_on_odd_and_even_samples() {
        assert_eq!(median_and_mad(&[3.0, 1.0, 2.0]), (2.0, 1.0));
        // Median 2.5; deviations 1.5, 0.5, 0.5, 97.5, so the MAD is 1.0.
        assert_eq!(median_and_mad(&[1.0, 2.0, 3.0, 100.0]), (2.5, 1.0));
    }

    /// Reverie's 15 instruction-length buckets, `1=` to `15=`, printed the way
    /// its `write_buckets` prints them: every bucket, zeros included.
    fn buckets(counts: [u64; 15]) -> String {
        counts
            .iter()
            .enumerate()
            .map(|(index, count)| format!("{}={count}", index + 1))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// A LiteInst record as Hermit prints it at reverie 9976e29c for today's
    /// tracer-hosted LiteInst, after the log line's prefix, with `direct`
    /// hooked calls and `trapped` first-site traps at one 2-byte site. The
    /// tracer patches that site by relocation and installs the patch through
    /// ptrace at its first-site trap, so the record counts the site as
    /// `relocated` and one `ptrace_installation` per trap. A hooked call needs
    /// an installed site, so every record here has a trap.
    /// The trailing `dispatch=` field is the tracer's projection onto the
    /// shared dispatch record: each hooked call returns through a `SIGTRAP`
    /// stop, so it counts there as `ptrace_sigtrap_stops`, the field's own
    /// `direct=` is 0, and the two `PTRACE_SYSCALL` stop counts are `n/a`.
    /// Only the `paths[...]` list is read; a parser that read `direct=`
    /// instead would get 0.
    fn liteinst_record(direct: u64, trapped: u64) -> String {
        let mut lengths = [0; 15];
        lengths[1] = 1;
        format!(
            "x DEBUG backend run complete backend=liteinst stats=LiteInst instrumentation stats: process_reports=0 distinct_rips_patched=1 patch_candidates=1 decisions[direct_pun=0,relocated=1,straddler_fallback=0,other_fallback=0] paths[first_site_seccomp={trapped},ptrace_installation={trapped},in_guest_sigsys=0,in_guest_nested_sigsys=0,in_guest_nested_hook=0,in_guest_physical_sigsys=0,fallback_completion_sigsys=0,cacheline_straddler=0,unpatchable_or_other=0,direct_hook={direct},fallback_refusal=0,patching_disabled=0] classified_candidates=1 cacheline_straddlers=0 non_straddling=1 instruction_lengths[{lengths}] straddle_prefix[{straddles}] dispatch=dispatch stats v1 backend=liteinst dispatches={stops} trapped={stops} direct=0 [signal_traps=0 patched_direct_calls=0 ptrace_seccomp_stops={trapped} ptrace_sigtrap_stops={direct} sigill_marker_hits=0 ptrace_syscall_entry_stops=n/a ptrace_syscall_exit_stops=n/a refusals=0] sites[candidates=1 patched=1 fell_back=0] processes=1",
            lengths = buckets(lengths),
            straddles = buckets([0; 15]),
            stops = direct + trapped
        )
    }

    /// A ptrace record as Hermit prints it at reverie 9976e29c, after the log
    /// line's prefix.
    const PTRACE_RECORD: &str = "x DEBUG backend run complete backend=ptrace stats=ptrace activity stats: tracees_started=1 stop_events=40 exited_tracees=1 seccomp_stops=38 signal_stops=0 exec_stops=1 child_stops[fork=0,vfork=0,clone=0] vfork_done_stops=0 dispatch=dispatch stats v1 backend=ptrace dispatches=38 trapped=38 direct=0 [signal_traps=0 patched_direct_calls=0 ptrace_seccomp_stops=38 ptrace_sigtrap_stops=0 sigill_marker_hits=0 ptrace_syscall_entry_stops=n/a ptrace_syscall_exit_stops=n/a refusals=0] sites[candidates=0 patched=0 fell_back=0] processes=1";

    #[test]
    fn check_stats_run_needs_one_record_for_its_own_backend() {
        let own = liteinst_record(100, 1);
        let other = PTRACE_RECORD.to_string();
        assert_eq!(
            check_stats_run("liteinst", &Outcome::Ok, &[own.clone()]),
            Ok(Some(100))
        );
        assert!(check_stats_run("liteinst", &Outcome::Ok, &[]).is_err());
        assert!(check_stats_run("liteinst", &Outcome::Ok, &[own.clone(), own.clone()]).is_err());
        assert!(check_stats_run("liteinst", &Outcome::Ok, &[other.clone()]).is_err());
        assert!(
            check_stats_run(
                "liteinst",
                &Outcome::Failed("exit 1".into()),
                &[own.clone()]
            )
            .is_err()
        );
        assert!(check_stats_run("liteinst", &Outcome::TimedOut, &[own]).is_err());
        // Backends other than LiteInst are not asked for dispatch paths.
        assert_eq!(check_stats_run("ptrace", &Outcome::Ok, &[other]), Ok(None));
    }

    #[test]
    fn check_stats_run_refuses_a_liteinst_record_without_a_direct_hook_count() {
        // A record without a readable paths list, or without direct_hook, is
        // refused rather than read as zero.
        let bare = "x DEBUG backend run complete backend=liteinst stats=LiteInst ...".to_string();
        assert!(check_stats_run("liteinst", &Outcome::Ok, &[bare]).is_err());
        let no_hook = "x DEBUG backend run complete backend=liteinst stats=LiteInst paths[first_site_seccomp=1]".to_string();
        assert!(
            check_stats_run("liteinst", &Outcome::Ok, &[no_hook])
                .unwrap_err()
                .contains("no direct_hook counter")
        );
    }

    #[test]
    fn check_hooked_calls_refuses_a_run_that_did_not_hook_the_site() {
        // Every call but the first took the hook: accepted.
        assert!(check_hooked_calls(100, 99, 0).is_ok());
        // Every call trapped: the slope would be a trap's cost.
        let trapped = check_hooked_calls(100, 0, 0);
        assert!(trapped.unwrap_err().contains("is 0, below 99"));
        // One call short of the floor is still refused.
        assert!(check_hooked_calls(100, 98, 0).is_err());
        // Hooks at start-up and exit sites do not count toward the floor:
        // ten of them in the zero-call run leave the same 98 short of 99.
        assert!(check_hooked_calls(100, 108, 10).is_err());
        assert!(check_hooked_calls(100, 109, 10).is_ok());
        // With a small N, hooks at other sites alone would have met the floor
        // (`--counts 0,5,10` with nine other hooks and an unpatched site).
        assert!(check_hooked_calls(10, 9, 9).is_err());
        // A zero-call run with more hooks than the measured run leaves none.
        assert!(check_hooked_calls(100, 5, 10).is_err());
        // The smallest run that owes the hook a call: one hooked call passes,
        // none is refused.
        assert!(check_hooked_calls(2, 1, 0).is_ok());
        assert!(check_hooked_calls(2, 0, 0).is_err());
        // A largest count below 2 owes the hook nothing, so even counts that
        // look clean are refused. `parse_options` no longer accepts such
        // counts (`--counts 0,1`), and this check does not rely on that.
        for calls in [0, 1] {
            let owed_nothing = check_hooked_calls(calls, calls, 0);
            assert!(
                owed_nothing
                    .unwrap_err()
                    .contains("owes no call to direct_hook")
            );
        }
    }

    #[test]
    fn stats_run_counts_put_the_liteinst_zero_call_run_first() {
        assert_eq!(stats_run_counts("liteinst", 100), vec![0u64, 100]);
        assert_eq!(stats_run_counts("ptrace", 100), vec![100u64]);
    }

    #[test]
    fn stats_verdict_checks_the_measured_run_against_the_zero_call_run() {
        // The zero-call run's 10 hooks reach the measured run's check: 109
        // less 10 meets the floor of 99, while 108 less 10 does not, although
        // 108 alone would.
        let mut baseline = None;
        let zero = [liteinst_record(10, 1)];
        assert_eq!(
            stats_verdict("liteinst", 0, &Outcome::Ok, &zero, &mut baseline),
            Ok(())
        );
        assert_eq!(baseline, Some(10));
        let enough = [liteinst_record(109, 1)];
        assert_eq!(
            stats_verdict("liteinst", 100, &Outcome::Ok, &enough, &mut baseline),
            Ok(())
        );
        let short = [liteinst_record(108, 1)];
        assert!(
            stats_verdict("liteinst", 100, &Outcome::Ok, &short, &mut baseline)
                .unwrap_err()
                .contains("less the zero-call run's 10")
        );

        // A zero-call run without a record, or one that failed, sets no
        // baseline, and the measured run is then refused however many hooks
        // it shows.
        let many = [liteinst_record(1000, 1)];
        for (outcome, records) in [
            (Outcome::Ok, Vec::new()),
            (Outcome::Failed("exit 1".into()), zero.to_vec()),
        ] {
            let mut baseline = None;
            assert!(stats_verdict("liteinst", 0, &outcome, &records, &mut baseline).is_err());
            assert_eq!(baseline, None);
            assert!(
                stats_verdict("liteinst", 100, &Outcome::Ok, &many, &mut baseline)
                    .unwrap_err()
                    .contains("zero-call run was rejected")
            );
        }

        // Other backends have no zero-call run and are judged on their own
        // record alone.
        let mut baseline = None;
        let ptrace = [PTRACE_RECORD.to_string()];
        assert_eq!(
            stats_verdict("ptrace", 100, &Outcome::Ok, &ptrace, &mut baseline),
            Ok(())
        );
        assert_eq!(baseline, None);
    }

    #[test]
    fn stats_accepted_judges_each_backend_on_its_own_runs() {
        let run = |backend: &str, verdict: Result<(), String>| -> StatsRun {
            (
                backend.to_string(),
                100,
                "ok".to_string(),
                Vec::new(),
                verdict,
            )
        };
        let runs = [
            run("liteinst", Ok(())),
            run("liteinst", Err("below the floor".to_string())),
            run("ptrace", Ok(())),
        ];
        assert_eq!(stats_accepted("liteinst", &runs), Some(false));
        assert_eq!(stats_accepted("ptrace", &runs), Some(true));
        assert_eq!(stats_accepted("native", &runs), None);
    }

    #[test]
    fn dispatch_paths_reads_the_paths_list_in_order() {
        let paths = dispatch_paths(&liteinst_record(7, 1)).expect("paths list");
        assert_eq!(
            paths,
            [
                ("first_site_seccomp".to_string(), 1),
                ("ptrace_installation".to_string(), 1),
                ("in_guest_sigsys".to_string(), 0),
                ("in_guest_nested_sigsys".to_string(), 0),
                ("in_guest_nested_hook".to_string(), 0),
                ("in_guest_physical_sigsys".to_string(), 0),
                ("fallback_completion_sigsys".to_string(), 0),
                ("cacheline_straddler".to_string(), 0),
                ("unpatchable_or_other".to_string(), 0),
                ("direct_hook".to_string(), 7),
                ("fallback_refusal".to_string(), 0),
                ("patching_disabled".to_string(), 0),
            ]
        );
        assert_eq!(dispatch_paths("no list here"), None);
        assert_eq!(dispatch_paths("x paths[direct_hook=many]"), None);
        assert_eq!(dispatch_paths("x paths[direct_hook=1"), None);
    }

    #[test]
    fn stats_records_keeps_only_marked_lines() {
        let stderr = format!("noise\n{PTRACE_RECORD}\nmore\n");
        assert_eq!(stats_records(&stderr), [PTRACE_RECORD]);
        assert!(stats_records("").is_empty());
    }

    #[test]
    fn hermit_argv_has_the_documented_shape() {
        let hermit = args(&["hermit"]);
        let fixture = Path::new("/f");
        assert_eq!(variant_argv(&hermit, "native", fixture, 3), ["/f", "3"]);
        assert_eq!(
            variant_argv(&hermit, "liteinst", fixture, 3),
            [
                "hermit",
                "--backend=liteinst",
                "--log=error",
                "run",
                "--base-env=minimal",
                "--env=LC_ALL=C",
                "--no-virtualize-cpuid",
                "--max-timeslice=disabled",
                "--",
                "/f",
                "3",
            ]
        );
    }
}
