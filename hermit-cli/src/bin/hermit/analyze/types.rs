/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A mode for analyzing a hermit run.

use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use clap::Parser;
use regex::Regex;
use reverie::PrettyBacktrace;
use reverie::process::ExitStatus;
use serde::Deserialize;
use serde::Serialize;
use tracing::metadata::LevelFilter;

/// Repeat a run multiple times in a controlled search to find concurrency bugs.
///
/// Hermit analyze searches over runs of `hermit run`, and its primary input is a set of CLI flags
/// that constitute a hermit run, i.e., ARGS in `hermit run ARGS`.  These arguments are passed to
/// all runs that analyze considers (but some options may be overwritten by the settings varied by
/// analyze's search process).
///
/// The analyzer is interested in programs with some *target* property (such as non-zero exit code
/// due to crashing).  Analyze must have, or find, a `hermit run` execution that exhibits this
/// target property. If the target "run1" is not explicitly provided, the tool can search for it
/// (--search).
///
/// Analyze must also have a second (baseline) one that does not. If a "run2" is not provided,
/// analyze assumes that a hermit run without chaos is the baseline.
///
/// Finally, given a target and a baseline run, the job of analyze is to search between them and
/// find the critical events: instructions which, when reordered, cause the target property to hold
/// or not.  This is evidence of an order violation or race condition bug, and means the the program
/// has at least one concurrency bug.
///
/// The critical events are identified to the user by printing their stack traces, optionally into a
/// separate report file.
#[derive(Debug, Parser, Clone)]
pub struct AnalyzeOpts {
    /// Target: Analyze runs that have collected stdout output matching this regular expression.
    #[clap(long, value_name = "REGEX")]
    pub target_stdout: Option<Regex>,

    /// Target: Analyze runs that have collected stderr output matching this regular expression.
    #[clap(long, value_name = "REGEX")]
    pub target_stderr: Option<Regex>,

    /// Target: Analyze runs that have the specified exit code, 0 to 255, as a shell reports it:
    /// the guest's own, or 128 plus the signal number when a signal killed it (134 for SIGABRT,
    /// which also matches a guest that exits 134).  Accepts "nonzero" for all nonzero exit codes.
    /// Accepts "none" or "any" for no filter at all (accepts any exit code).  The default is
    /// "nonzero" because it's very common to analyze a bug that causes the program to crash or
    /// error.
    #[clap(long, default_value = "nonzero", value_name = "NUM|nonzero|any")]
    pub target_exit_code: ExitStatusConstraint,

    /// The trials' log level, as --trial-log sets it, which wins when both are given. This
    /// also accepts `error` and `trace`; a level above warn writes each trial's log into its
    /// own file in the workspace. Errors are kept at `off` too, as with --trial-log=off.
    #[clap(short, long, value_name = "LEVEL", env = "HERMIT_LOG")]
    pub guest_log: Option<LevelFilter>,

    /// Whether `guest_log` came from its environment variable, HERMIT_LOG, which the
    /// workspace's retention names.
    #[clap(skip)]
    pub guest_log_from_env: bool,

    /// Insist on perfect determinism before proceeding with the analysis.
    #[clap(long)]
    pub selfcheck: bool,

    /// If the first run doesn't match the target criteria, search for one that does.
    #[clap(long)]
    pub search: bool,

    /// If the first run doesn't match the target criteria, search for one that does.
    #[clap(long)]
    pub run_needleman: bool,

    /// Before searching, minimize the target run's chaos-mode interventions (its preemptions
    /// and priority changes) to those it needs to meet the target criteria, by knocking them out
    /// a batch at a time. This may accelerate the subsequent binary search.
    ///
    /// The baseline is then the minimized record with its latest intervention knocked out, and
    /// knocked out again while the replay still meets the target. When no intervention is left,
    /// there is no baseline to bisect against and the analysis ends with that error, as it does
    /// with the run's own error for a knocked-out run that does not finish; `--prefix-search`
    /// needs no baseline. So `--minimize` finds its own baseline and does not
    /// take `--run2-seed`, `--run2-preemptions` or `--run2-schedule`. To search between a given
    /// pair of runs, omit `--minimize`.
    #[clap(
        long,
        conflicts_with = "run2_seed",
        conflicts_with = "run2_preemptions",
        conflicts_with = "run2_schedule"
    )]
    pub minimize: bool,

    /// Search within the target run's schedule instead of between it and a baseline run.
    ///
    /// Each trial replays the first K events of the target schedule and then lets the
    /// scheduler continue without chaos. Replaying none of it gives the baseline outcome and
    /// replaying all of it the target outcome; a binary search finds the K at which the
    /// outcome flips. Event #K is the scheduling decision that commits the run to the target:
    /// the report shows the stack of the thread the target schedule runs there and of the
    /// thread the scheduler would run instead. Each stack is taken where the trial's replay
    /// consumes the recorded event at position K, however many polling retries the trial
    /// made before it.
    ///
    /// Unlike the default search, it needs no baseline schedule, so events that only one of
    /// two unrelated runs executed (timeslices that end at other points, iterations of a
    /// polling loop) cannot stop it. It takes about log2(schedule length) trials, two more for
    /// each continuation (`--continuation-timeslice`) it tries, and two to recheck the
    /// boundary.
    #[clap(
        long,
        conflicts_with = "minimize",
        conflicts_with = "run2_seed",
        conflicts_with = "run2_preemptions",
        conflicts_with = "run2_schedule",
        conflicts_with = "run_needleman"
    )]
    pub prefix_search: bool,

    /// The timeslice, in virtual nanoseconds, with which `--prefix-search` trials continue
    /// without chaos after their replayed prefix: `auto` (the default) or a number.
    ///
    /// A race found only at a short timeslice often shows without chaos at that timeslice too:
    /// the scheduler alone, replaying none of the target schedule, already meets the target,
    /// and the search has nothing to bisect against. With `auto` the search continues at the
    /// run's own `--max-timeslice`, and when that cannot serve (the scheduler alone meets the
    /// target, or replaying the whole schedule misses it) at hermit's default 200 ms. This also
    /// applies to the prefix search the default search falls back to. The report names the
    /// timeslice the search used.
    ///
    /// The value is the trials' `--max-timeslice`, which also ends the replayed prefix's
    /// slices that its schedule does not end, so it may not be shorter than the run's own, and
    /// a run whose arguments disable PMU timeslices takes none.
    #[clap(long, value_name = "NANOS|auto", default_value = "auto")]
    pub continuation_timeslice: ContinuationTimeslice,

    /// Use `--imprecise-timers` during the (chaos) search phase. Only has an effect if search is
    /// enabled.
    #[clap(long)]
    pub imprecise_search: bool,

    /// Identify the target execution by chaos seed (hermit run --seed).
    ///
    /// It is an error if this execution does not meet the indicated target criteria.
    #[clap(
        long,
        conflicts_with = "run1_preemptions",
        conflicts_with = "run1_schedule",
        value_name = "NUM"
    )]
    pub run1_seed: Option<u64>,

    /// Load target execution from hermit run --record-preemptions-to.
    ///
    /// Analyze replays the record as the target run. It is an error if that replay does not meet
    /// the indicated target criteria.
    #[clap(
        long,
        conflicts_with = "run1_seed",
        conflicts_with = "run1_schedule",
        value_name = "PATH"
    )]
    pub run1_preemptions: Option<PathBuf>,

    /// Not implemented: `hermit analyze` refuses this option (exit 122).
    /// Identify the target run with --run1-preemptions or --run1-seed.
    #[clap(
        long,
        conflicts_with = "run1_seed",
        conflicts_with = "run1_preemptions",
        value_name = "PATH"
    )]
    pub run1_schedule: Option<PathBuf>,

    /// Optionally use a second chaos seed to identify the baseline run that
    /// does NOT meet the target criteria.
    #[clap(
        long,
        conflicts_with = "run2_preemptions",
        conflicts_with = "run2_schedule",
        value_name = "NUM"
    )]
    pub run2_seed: Option<u64>,

    /// Load baseline execution from hermit run --record-preemptions-to.
    #[clap(
        long,
        conflicts_with = "run2_seed",
        conflicts_with = "run2_schedule",
        value_name = "PATH"
    )]
    pub run2_preemptions: Option<PathBuf>,

    /// Not implemented: `hermit analyze` refuses this option (exit 122).
    /// Identify the baseline run with --run2-preemptions or --run2-seed.
    #[clap(
        long,
        conflicts_with = "run2_seed",
        conflicts_with = "run2_preemptions",
        value_name = "PATH"
    )]
    pub run2_schedule: Option<PathBuf>,

    /// A path to write the final analyze result, showing the critical event stack traces.
    ///
    /// Otherwise it is written to stdout.
    #[clap(long)]
    pub report_file: Option<PathBuf>,

    // TODO: run2_schedule
    //
    /// Use to seed the PRNG that supplies randomness to the analyzer when it is making random
    /// decisions during search or minimization.  If unset, then system randomness is used.
    #[clap(long)]
    pub analyze_seed: Option<u64>,

    /// Print quite a bit of extra information so that you can see exactly what is happening:
    /// every run's reproducer command, the guest's output and the search's steps.
    ///
    /// Analyze keeps the workspace when it exits, as --keep-tmp does, so that the files the
    /// reproducers name can be read. It does not raise the trials' log level: --trial-log
    /// does.
    #[clap(long, short)]
    pub verbose: bool,

    /// How much each run analyze starts logs, and where: `warn` (the default), `off`, `info`
    /// or `debug`.
    ///
    /// At `warn` a run's warnings and errors go to analyze's stderr, as those of `hermit run`
    /// do, and `off` drops the warnings: errors still go there, since the reason Hermit gives
    /// when it refuses a trial is one. At `info` or `debug` each run writes its log, at that level,
    /// into a file of its own in the workspace (`<run>.log`, see --tmp-dir), and analyze keeps
    /// the workspace when it exits, as --keep-tmp does, so that the logs can be read. A debug
    /// log can grow to gigabytes per run and make the run several times slower, so a trial a
    /// bound stops then gets through less of the program. --selfcheck logs the runs whose logs
    /// it compares (the target run, the --search run that becomes it, the runs of --minimize
    /// and the target's replay) at debug or above, into their files, whatever this says.
    /// When this option is not given, --guest-log gives the level, if given.
    #[clap(long, value_name = "LEVEL", value_enum)]
    pub trial_log: Option<TrialLog>,

    /// The maximum amount of schedule jitter (deviation) to tolerater while performing the bisection search.
    #[clap(long)]
    pub jitter_dist: Option<usize>,

    /// Number of events to print before/after the critical events found, when showing the execution
    /// context along weth the analyze report.
    #[clap(long, short, value_name = "EVTCNT", default_value = "5")]
    pub execution_context: usize,

    /// Directory under which the analysis creates its workspace of temporary artifacts
    /// (schedules, logs, reports of every run), created if missing.
    ///
    /// By default the workspace is created in the system's temporary directory: $TMPDIR, or
    /// /tmp when TMPDIR is unset. Analyze removes the workspace when it exits, unless
    /// --keep-tmp, --verbose, or --trial-log above warn, or an error ended it; this directory
    /// itself stays.
    #[clap(long, value_name = "PATH")]
    pub tmp_dir: Option<PathBuf>,

    /// Wall-clock bound on each run after the initial target attempt (and, when explicit, on the
    /// replay of a `--run1-preemptions` record): a positive number of seconds, `auto` (the
    /// default), or `off`.
    ///
    /// With `auto`, the initial run and completed target/baseline endpoint runs are measured.
    /// Later runs are bounded at max(60 s, 10 times the slowest measured run), rounded up to
    /// whole seconds. With `--run1-preemptions`, the initial run is a replay of the record: it
    /// is measured like a seed's, and an explicit number bounds it too.
    /// A trial that reaches its bound stops analysis with exit 124: it establishes no outcome
    /// and cannot support a successful report. Retry with a larger bound if the guest can
    /// finish. A `--timeout` in the run arguments also bounds the initial run and wins wherever
    /// it is shorter. `off` removes only analyze's bounds, not the run arguments' timeout.
    ///
    /// The bound needs a backend on which `hermit run --timeout` is qualified; elsewhere `auto`
    /// gives no bound and an explicit number is refused. Use an outer timeout there.
    /// While trials are bounded, the log files they write under `--trial-log` above warn or
    /// `--selfcheck` are capped at max(64 MiB, 10 times the largest measured endpoint log),
    /// within HERMIT_LOG_MAX_BYTES.
    /// HERMIT_LOG_MAX_BYTES=0 lifts both file caps; `off` lifts analyze's file cap.
    #[clap(long, value_name = "SECONDS|auto|off", default_value = "auto")]
    pub trial_timeout: TrialTimeout,

    /// The most memory analyze's own process may hold (the schedules and the
    /// search; each trial runs in a child process and is not counted): a size
    /// such as `16G` or `512M` (K, M, G and T are powers of 1024) or a byte
    /// count; `auto`, a quarter of the memory it may use (the host's, or its
    /// cgroup's limit if lower); or `off`. A watchdog thread reads the
    /// process's own memory, resident or swapped out, ten times a second and
    /// ends analyze with exit code 122 once it is over the budget, instead of
    /// letting it grow until the kernel kills it.
    #[clap(long, value_name = "SIZE|auto|off", default_value = "auto")]
    pub max_memory: super::MemoryBudget,

    /// Keep the workspace (see --tmp-dir) when analyze exits.
    ///
    /// The workspace holds every run's schedules, logs, output and reproducer command, and the
    /// paths analyze prints name it. By default analyze removes it when it exits after its
    /// report, and when SIGTERM, SIGINT or SIGHUP stops it; after an error it keeps it, since
    /// the error can name trial files in it. A stop signal ends the run in progress and starts
    /// no other; analyze then removes the workspace and dies of that signal (a shell reports
    /// 128 plus its number, 143 for SIGTERM). --verbose, and --trial-log above warn, keep the
    /// workspace too. The report file (--report-file) is never removed. Without --keep-tmp, even
    /// with --verbose, a bisection trial's schedules are removed as soon as its outcome is read,
    /// and every run's recording with event times once analyze has written it without them.
    ///
    /// Analyze cannot clean up after SIGKILL, nor after a stop signal it does not handle: one it
    /// inherits as ignored stays ignored, and an analyze that is PID 1 of its PID namespace
    /// (`docker run IMAGE hermit analyze ...` without --init) leaves the stop signals at their
    /// default actions, which Linux discards for a namespace's init. Neither does it handle
    /// them, so that they kill it at once as before, when /proc is not its own PID namespace's
    /// (analyze started by another process inside `unshare -pf` without --mount-proc), when
    /// it inherited more than 64 children, or where it cannot open, signal through or wait on
    /// a pidfd (Linux before 5.4, or a seccomp filter), through which alone it signals the run.
    /// A stop it cannot pass on to the run (no file descriptor left) makes it die of the
    /// signal as without its handler, saying why; the run dies with it, and the workspace and
    /// the running trial's private temporary directory stay.
    #[clap(long)]
    pub keep_tmp: bool,

    /// Specify that analyze itself should return a non-zero exit code on success.
    /// This is needed under esoteric invocation scenarios.
    #[clap(long, value_name = "INT32")]
    pub success_exit_code: Option<i32>,

    /// Additional arguments to pass through unmodified to the hermit run subcommand.  While
    /// arguments are passed through to hermit run even without this, this explicit flag is useful
    /// for intermixing flags `hermit run` among the analyze args, in spite of them starting with
    /// hyphens.
    #[clap(long, short = 'a')]
    pub run_arg: Vec<String>,

    /// A full set of CLI arguments for the original `hermit run` to analyze.
    #[clap(value_name = "ARGS")]
    pub run_args: Vec<String>,

    /// The backend every trial runs on, taken from the global option
    /// (`hermit --backend <BACKEND> analyze ...`) by [`AnalyzeOpts::main`].
    /// `run` has no `--backend` of its own, so this is the only way to choose it.
    #[clap(skip)]
    pub backend: Option<hermit::Backend>,

    /// The invocation's `--max-log-bytes` limit and its ONE shared budget,
    /// which every trial charges; taken from the global options by
    /// [`AnalyzeOpts::main`].
    #[clap(skip)]
    pub max_log_bytes: Option<u64>,
    #[clap(skip)]
    pub log_budget: Option<crate::app::tracing::LogBudget>,

    /// Why this host's retired-branch counter is inexact, if it is; handed to
    /// every trial's `RunOpts` for the `--strict` refusal and the warning
    /// without `--strict`. Tests replace it to reach both on a host whose
    /// counters pass.
    #[clap(skip = crate::app::host_capabilities::host_inexact_branch_counter as fn() -> Option<String>)]
    pub inexact_branch_counter: fn() -> Option<String>,

    /// The branch-counter warning `admit_branch_counter` printed before the
    /// first trial, without its newline, for the final report to repeat
    /// (`Report::branch_counter_warning`); `None` when it printed none.
    #[clap(skip)]
    pub branch_counter_warning: Option<String>,

    /// What the endpoint runs measured so far; the trials' bounds derive
    /// from it ([`AnalyzeOpts::trial_bound`]). Every `RunData` copies these
    /// options when it is created, so a run takes the bound of the endpoints
    /// measured before it.
    #[clap(skip)]
    pub endpoints: EndpointMeasurements,
    /// The jumps of global time of every recorded schedule this invocation
    /// read, which the trial schedules it writes carry
    /// (`crate::app::time_skip_carry`). Shared by every copy of these options.
    #[clap(skip)]
    pub time_skip_sources: crate::app::time_skip_carry::SharedTimeSkipSources,
}

/// `--continuation-timeslice`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationTimeslice {
    /// Short first, then coarser where the short one cannot serve.
    Auto,
    /// This many virtual nanoseconds.
    Nanos(NonZeroU64),
}

impl FromStr for ContinuationTimeslice {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "auto" {
            return Ok(ContinuationTimeslice::Auto);
        }
        match s.parse::<NonZeroU64>() {
            Ok(nanos) if nanos.get() >= detcore_model::time::NANOS_PER_RCB as u64 => {
                Ok(ContinuationTimeslice::Nanos(nanos))
            }
            _ => Err(format!(
                "expected `auto` or a timeslice of at least one RCB ({} virtual nanoseconds), \
                 got {s:?}",
                detcore_model::time::NANOS_PER_RCB
            )),
        }
    }
}

/// The least a run analyze starts logs: errors, among them the reason Hermit
/// gives when it refuses the run at an operation it does not support, which
/// the refusal's warning points at.
pub const TRIAL_LOG_LEAST: LevelFilter = LevelFilter::ERROR;

/// `--trial-log`: how much each run analyze starts logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TrialLog {
    /// Errors only, to analyze's stderr: the reason Hermit refuses a trial is one.
    Off,
    /// Warnings and errors, to analyze's stderr.
    Warn,
    /// Info and above, into the run's own log file.
    Info,
    /// Debug and above, into the run's own log file.
    Debug,
}

impl TrialLog {
    /// The level a run logs at. `off` keeps errors ([`TRIAL_LOG_LEAST`]).
    pub fn level(self) -> LevelFilter {
        match self {
            TrialLog::Off => TRIAL_LOG_LEAST,
            TrialLog::Warn => LevelFilter::WARN,
            TrialLog::Info => LevelFilter::INFO,
            TrialLog::Debug => LevelFilter::DEBUG,
        }
    }
}

/// `--trial-timeout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialTimeout {
    /// Derived from the endpoint runs' wall-clock time.
    Auto,
    /// No bound beyond the run arguments' own `--timeout`.
    Off,
    /// This many seconds.
    Seconds(NonZeroU64),
}

impl FromStr for TrialTimeout {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(TrialTimeout::Auto),
            "off" => Ok(TrialTimeout::Off),
            _ => s.parse().map(TrialTimeout::Seconds).map_err(|_| {
                format!("expected a positive number of seconds, `auto` or `off`, got {s:?}")
            }),
        }
    }
}

/// The least wall-clock bound `--trial-timeout=auto` gives a trial.
pub const TRIAL_TIMEOUT_FLOOR: Duration = Duration::from_secs(60);
/// How many times the slowest endpoint run `--trial-timeout=auto` gives a trial.
pub const TRIAL_TIMEOUT_FACTOR: u32 = 10;
/// The least cap on a trial's log file.
pub const TRIAL_LOG_FLOOR: u64 = 64 << 20;
/// How many times the largest endpoint log a trial's log file may grow to.
pub const TRIAL_LOG_FACTOR: u64 = 10;

/// The slowest wall-clock time and the largest log file of the endpoint runs
/// measured so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EndpointMeasurements {
    /// The slowest endpoint run, if one has been measured.
    pub slowest: Option<Duration>,
    /// The largest endpoint log file, if an endpoint run wrote one.
    pub largest_log: Option<u64>,
}

impl EndpointMeasurements {
    /// Record one endpoint run. Returns true if it is the slowest so far.
    pub fn record(&mut self, wall: Duration, log_bytes: Option<u64>) -> bool {
        if let Some(bytes) = log_bytes {
            self.largest_log = self.largest_log.max(Some(bytes));
        }
        let slowest = self.slowest.is_none_or(|slowest| wall > slowest);
        if slowest {
            self.slowest = Some(wall);
        }
        slowest
    }
}

// TODO: introduce a new type to encapsulate the state of the search, and make it immutable.
// pub struct SearchState {}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum ExitStatusConstraint {
    /// Accept only a specific exit code.
    Exact(i32),
    /// Accept any nonzero exit code.
    NonZero,
    /// Accept any exit code.  No filter.
    Any,
}

/// The exit code a shell reports for a run that ended with `status`: the
/// guest's own code, or 128 plus the signal number for a signal death. Not
/// the raw wait status, which shifts an exit code left by eight bits.
pub fn run_exit_code(status: ExitStatus) -> i32 {
    match status {
        ExitStatus::Exited(code) => code,
        ExitStatus::Signaled(signal, _) => detcore_model::signal_exit_status(signal as i32),
        ExitStatus::SignaledRaw(signal, _) => detcore_model::signal_exit_status(signal),
    }
}

impl ExitStatusConstraint {
    /// Is the constraint met for a run that ended with `exit_status`
    /// ([`run_exit_code`])?
    pub fn is_match(&self, exit_status: ExitStatus) -> bool {
        let exit_code = run_exit_code(exit_status);
        match self {
            ExitStatusConstraint::Exact(code) => exit_code == *code,
            ExitStatusConstraint::NonZero => exit_code != 0,
            ExitStatusConstraint::Any => true,
        }
    }
}

impl FromStr for ExitStatusConstraint {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(n) = s.parse::<i32>() {
            if !(0..=255).contains(&n) {
                return Err(format!(
                    "Exit code constraint {n} is out of range: an exit code is 0 to 255 (128 plus \
                     the signal number for a signal death)"
                ));
            }
            Ok(ExitStatusConstraint::Exact(n))
        } else {
            match s.to_lowercase().as_str() {
                "nonzero" => Ok(ExitStatusConstraint::NonZero),
                "none" | "any" => Ok(ExitStatusConstraint::Any),
                _ => Err(format!(
                    "Unable to parse string as exit code constraint, expected a number, 'none'/'any', or 'nonzero'.  Received: {}",
                    s
                )),
            }
        }
    }
}

/// The final report that comes out of the analyze process.
#[derive(PartialEq, Debug, Eq, Clone, Hash, Serialize, Deserialize)]
pub struct Report {
    /// A header containing some contextual information, to go at the beginning of the report.
    pub header: String,

    /// Some additional context about where in the execution the issue occured. (Basically used as a footer.)
    pub additional_context: String,

    /// Is it a baseline run (e.g. passing run) vs a target run (e.g. failing/crashing).
    pub baseline_run: bool,

    /// The runtime context for one critical event, which is racing with, and does not commute with
    /// the other.
    pub critical_event1: ReportCriticalEvent,
    /// The runtime context for the other identified critical event.
    pub critical_event2: ReportCriticalEvent,

    /// The warning line, without its newline, that `hermit analyze` or `hermit bisect` printed
    /// before its first trial because those trials read a retired-branch counter that failed
    /// Reverie's validation (`AnalyzeOpts::admit_branch_counter`): the seeds, preemption records
    /// and schedules behind this report are then not guaranteed to reproduce it. Absent where the
    /// counter passed or the trials do not read it, as in every report written before the field
    /// existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_counter_warning: Option<String>,

    /// `--prefix-search` only, when the deciding event is a timing decision
    /// (both continuations run the same thread there, for different lengths):
    /// that thread's stack at the deciding event. The two critical events are
    /// then the threads that run next in each continuation, and their
    /// `event_index` is a position in that continuation's recorded schedule
    /// (the header names both files); where no other thread runs, it is that
    /// schedule's length and the stack is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deciding_event: Option<ReportCriticalEvent>,
}

/// Runtime context for a critical event included in the analyze report.
#[derive(PartialEq, Debug, Eq, Clone, Hash, Serialize, Deserialize)]
pub struct ReportCriticalEvent {
    /// The event index.
    pub event_index: usize,

    /// The thread stack trace.
    pub stack: Option<PrettyBacktrace>,
}
