/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Locate the scheduling event that changes a program from passing to failing.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use detcore::preemptions::PreemptionRecord;
use detcore::types::SchedEvent;
use hermit::Error;
use regex::Regex;
use reverie::process::ExitStatus;
use tracing::metadata::LevelFilter;

use super::analyze::AnalyzeOpts;
use super::analyze::ExitStatusConstraint;
use super::global_opts::GlobalOpts;

/// Bisect two recorded schedules to identify the event ordering that causes a failure.
#[derive(Debug, Parser)]
pub struct BisectOpts {
    /// A recorded schedule whose replay succeeds.
    #[clap(long, value_name = "SCHEDULE")]
    good: PathBuf,

    /// A recorded schedule whose replay exhibits the target failure.
    #[clap(long, value_name = "SCHEDULE")]
    bad: PathBuf,

    /// Treat stdout matching this regular expression as part of the target failure.
    #[clap(long, value_name = "REGEX")]
    target_stdout: Option<Regex>,

    /// Treat stderr matching this regular expression as part of the target failure.
    #[clap(long, value_name = "REGEX")]
    target_stderr: Option<Regex>,

    /// Exit status identifying the target failure.
    #[clap(long, default_value = "nonzero", value_name = "NUM|nonzero|any")]
    target_exit_code: ExitStatusConstraint,

    /// Logging level for replayed guest runs.
    #[clap(short, long, value_name = "LEVEL", env = "HERMIT_LOG")]
    guest_log: Option<LevelFilter>,

    /// Write the machine-readable race report to this path.
    #[clap(long, value_name = "PATH")]
    report_file: Option<PathBuf>,

    /// Use Needleman-Wunsch alignment while selecting midpoint schedules.
    #[clap(long)]
    needleman: bool,

    /// Maximum accepted edit-distance jitter in a realized replay schedule.
    #[clap(long, value_name = "EVENTS")]
    jitter_dist: Option<usize>,

    /// Number of schedule events to show around the localized race.
    #[clap(long, value_name = "EVENTS", default_value = "5")]
    execution_context: usize,

    /// Print replay commands and guest output for each bisection step.
    #[clap(long, short)]
    verbose: bool,

    /// Arguments for the underlying `hermit run`, followed by the program and its arguments.
    #[clap(value_name = "RUN_ARGS", required = true)]
    run_args: Vec<String>,
}

impl BisectOpts {
    pub fn main(&self, _global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let good = read_schedule(&self.good, "good")?;
        let bad = read_schedule(&self.bad, "bad")?;
        if good == bad {
            bail!("the --good and --bad schedules contain identical event traces");
        }

        let mut analyzer = AnalyzeOpts {
            target_stdout: self.target_stdout.clone(),
            target_stderr: self.target_stderr.clone(),
            target_exit_code: self.target_exit_code.clone(),
            guest_log: self.guest_log,
            selfcheck: false,
            search: false,
            run_needleman: self.needleman,
            minimize: false,
            imprecise_search: false,
            run1_seed: None,
            run1_preemptions: None,
            run1_schedule: None,
            run2_seed: None,
            run2_preemptions: None,
            run2_schedule: None,
            report_file: self.report_file.clone(),
            analyze_seed: None,
            verbose: self.verbose,
            jitter_dist: self.jitter_dist,
            execution_context: self.execution_context,
            tmp_dir: None,
            success_exit_code: None,
            run_arg: Vec::new(),
            run_args: self.run_args.clone(),
        };

        analyzer.bisect_schedule_pair(good, bad)
    }
}

fn read_schedule(path: &Path, label: &str) -> anyhow::Result<Vec<SchedEvent>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read --{label} schedule {}", path.display()))?;
    let record: PreemptionRecord = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse --{label} schedule {}", path.display()))?;
    record
        .validate()
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("--{label} schedule {} failed validation", path.display()))?;
    if !record.contains_schedevents() {
        bail!(
            "--{label} schedule {} contains no global schedule events",
            path.display()
        );
    }
    Ok(record.into_global())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_fixture_contains_events() {
        let schedule = read_schedule(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test-resources/flaky_cas_sequence_schedules-passing.json"),
            "good",
        )
        .unwrap();
        assert!(!schedule.is_empty());
    }
}
