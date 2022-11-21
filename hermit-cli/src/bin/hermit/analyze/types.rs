/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A mode for analyzing a hermit run.

use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;
use regex::Regex;
use reverie::process::ExitStatus;
use serde::Deserialize;
use serde::Serialize;

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
///
#[derive(Debug, Parser)]
pub struct AnalyzeOpts {
    /// Target: Analyze runs that have collected stdout output matching this regular expression.
    #[clap(long, value_name = "REGEX")]
    pub target_stdout: Option<Regex>,

    /// Target: Analyze runs that have collected stderr output matching this regular expression.
    #[clap(long, value_name = "REGEX")]
    pub target_stderr: Option<Regex>,

    /// Target: Analyze runs that have the specified exit code.  Accepts "nonzero" for all nonzero
    /// exit codes.  Accepts "none" or "any" for no filter at all (accepts any exit code).  The
    /// default is "nonzero" because it's very common to analyze a bug that causes the program to
    /// crash or error.
    #[clap(long, default_value = "nonzero", value_name = "NUM|nonzero|any")]
    pub target_exit_code: ExitStatusConstraint,

    /// Insist on perfect determinism before proceeding with the analysis.
    #[clap(long)]
    pub selfcheck: bool,

    /// If the first run doesn't match the target criteria, search for one that does.
    #[clap(long)]
    pub search: bool,

    /// Given a passing/failing run pair, based on different chaos seeds, first minimize the
    /// chaos-mode interventions necessary to flip between the two outcomes.  This may accelerate
    /// the subsequent binary search.
    #[clap(
        long,
        conflicts_with = "run2-seed",
        conflicts_with = "run2-preemptions",
        conflicts_with = "run2-schedule"
    )]
    pub minimize: bool,

    /// Use `--imprecise-timers` during the (chaos) search phase. Only has an effect if search is
    /// enabled.
    #[clap(long)]
    pub imprecise_search: bool,

    /// Identify the target execution by chaos seed (hermit run --seed).
    ///
    /// It is an error if this execution does not meet the indicated target criteria.
    #[clap(
        long,
        conflicts_with = "run1-preemptions",
        conflicts_with = "run1-schedule",
        value_name = "NUM"
    )]
    pub run1_seed: Option<u64>,

    /// Load target execution from hermit run --record-preemptions-to.
    #[clap(
        long,
        conflicts_with = "run1-seed",
        conflicts_with = "run1-schedule",
        value_name = "PATH"
    )]
    pub run1_preemptions: Option<PathBuf>,

    /// Load target execution from hermit run --record-schedule-to.
    #[clap(
        long,
        conflicts_with = "run1-seed",
        conflicts_with = "run1-preemptions",
        value_name = "PATH"
    )]
    pub run1_schedule: Option<PathBuf>,

    /// Optionally use a second chaos seed to identify the baseline run that
    /// does NOT meet the target criteria.
    #[clap(
        long,
        conflicts_with = "run2-preemptions",
        conflicts_with = "run2-schedule",
        value_name = "NUM"
    )]
    pub run2_seed: Option<u64>,

    /// Load baseline execution from hermit run --record-preemptions-to.
    #[clap(
        long,
        conflicts_with = "run2-seed",
        conflicts_with = "run2-schedule",
        value_name = "PATH"
    )]
    pub run2_preemptions: Option<PathBuf>,

    /// Load baseline execution from hermit run --record-schedule-to.
    #[clap(
        long,
        conflicts_with = "run2-seed",
        conflicts_with = "run2-preemptions",
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

    /// Print quite a bit of extra information so that you can see exactly what is happening.
    #[clap(long, short)]
    pub verbose: bool,

    /// Where the analysis run will store its temporary artifacts.
    ///
    /// By default this is a directory in `/tmp`
    #[clap(long, value_name = "PATH")]
    pub tmp_dir: Option<PathBuf>,

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
}

// TODO: introduce a new type to encapsulate the state of the search, and make it immutable.
// pub struct SearchState {}

#[derive(Debug, Eq, PartialEq)]
pub enum ExitStatusConstraint {
    /// Accept only a specific exit code.
    Exact(i32),
    /// Accept any nonzero exit code.
    NonZero,
    /// Accept any exit code.  No filter.
    Any,
}

impl ExitStatusConstraint {
    /// Is the constraint met for a given exit code?
    pub fn is_match(&self, exit_status: ExitStatus) -> bool {
        let exit_code = exit_status.into_raw();
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
#[derive(PartialEq, Default, Debug, Eq, Clone, Hash, Serialize, Deserialize)]
pub struct Report {
    /// Any additional context about the error detected.
    pub header: String,
    /// The runtime context for one critical event, which is racing with, and does not commute with
    /// the other.
    pub stack1: String,
    /// The runtime context for the other identified critical event.
    pub stack2: String,
}
