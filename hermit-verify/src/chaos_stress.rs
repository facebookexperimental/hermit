/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::BufRead as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Instant;

use clap::Parser;
use colored::Colorize;
use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
use rand::RngExt as _;

use crate::CommonOpts;
use crate::cli_wrapper::display_cmd;
use crate::cli_wrapper::*;

/// Attempts at one iteration while Hermit refuses its runs for PMU skid
/// overshoot.
const SKID_ATTEMPTS: u32 = 4;

/// The prefix of the line Hermit prints when it refuses a run for PMU skid
/// overshoot (`classify_failure` in hermit-cli/src/bin/hermit/main.rs).
const SKID_OVERSHOOT_REFUSAL_PREFIX: &str =
    "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=";

/// Hermit's own outcomes that no guest produced, each the exit status from the
/// allocation table in hermit-cli/src/lib.rs with the stderr marker that tells
/// it apart from a guest choosing the same status. A run that ends in one of
/// these says nothing about the guest.
const HERMIT_OUTCOMES: [(i32, &str); 5] = [
    (HERMIT_POLICY_REFUSAL_EXIT, "HERMIT_POLICY_REFUSAL "),
    (123, "HERMIT_LOG_CAP "),
    (124, "HERMIT_RUN_TIMEOUT "),
    (124, "HERMIT_RUN_TIMEOUT_FALLBACK"),
    (125, "HERMIT_INTERNAL_FAILURE "),
];

/// Verification utility for replaying preemptions under hermit
/// This utility runs a guest program under "hermit run" and records schedules and preemptions. On the second run those recorded schedules are replayed via "hermit run" subcommand
/// The outputs (stdout, stderr, log file, schedules) are captured and placed in a temporary directory allowing further inspection (if --keep-temp-dir is provided)
#[derive(Parser, Debug)]
pub struct ChaosStressOpts {
    // Maximum number of iterations performed during the test
    #[clap(long, default_value = "100")]
    max_iterations_count: i32,
    /// Path to a guest program
    #[clap(value_name = "PROGRAM")]
    guest_program: PathBuf,
    /// Additional argument for hermit run subcommand
    #[clap(long)]
    hermit_arg: Vec<String>,
    /// Arguments for a guest program
    #[clap(value_name = "ARGS")]
    args: Vec<String>,
}

/// What one chaos run showed about the guest.
#[derive(Debug, PartialEq, Eq)]
enum RunOutcome {
    /// The guest exited 0.
    Passed,
    /// The guest exited nonzero or died of a signal.
    Failed,
    /// Hermit refused the run for PMU skid overshoot, which measures the
    /// host's interrupt latency rather than the guest
    /// (<https://github.com/rrnewton/hermit/issues/1845>). The refusal line.
    SkidRefused(String),
}

/// Classify one finished `hermit run` by its exit status and stderr. Only the
/// guest's own outcome counts: a skid refusal is returned for a retry, and any
/// other outcome of Hermit's own ([`HERMIT_OUTCOMES`]) is an error, because
/// counting it as the guest failing would report a race nobody found
/// (<https://github.com/rrnewton/hermit/issues/4490>).
///
/// Hermit raises a guest's fatal signal on itself, so a signal death is the
/// guest failing.
fn classify(status: ExitStatus, stderr: &str) -> anyhow::Result<RunOutcome> {
    let Some(code) = status.code() else {
        return Ok(RunOutcome::Failed);
    };
    let marker = |line: &str| {
        HERMIT_OUTCOMES
            .iter()
            .any(|(status, prefix)| *status == code && line.starts_with(prefix))
    };
    let hermit_lines: Vec<&str> = stderr
        .lines()
        .map(str::trim_end)
        .filter(|line| marker(line))
        .collect();
    match hermit_lines.as_slice() {
        [] if code == 0 => Ok(RunOutcome::Passed),
        [] => Ok(RunOutcome::Failed),
        [refusal] if is_skid_refusal(refusal, stderr) => {
            Ok(RunOutcome::SkidRefused(refusal.to_string()))
        }
        lines => anyhow::bail!(
            "Hermit itself ended the run with status {code}, so it says nothing about the guest:\n{}",
            lines.join("\n")
        ),
    }
}

/// Whether REFUSAL is the typed skid-overshoot refusal: a positive count, and
/// Hermit's `HERMIT_SKID_OVERSHOOT` report of the late interrupt beside it
/// with no panic.
fn is_skid_refusal(refusal: &str, stderr: &str) -> bool {
    let counted = refusal
        .strip_prefix(SKID_OVERSHOOT_REFUSAL_PREFIX)
        .is_some_and(|count| {
            count.bytes().all(|byte| byte.is_ascii_digit())
                && count.parse::<u64>().is_ok_and(|count| count > 0)
        });
    counted
        && stderr
            .lines()
            .any(|line| line.starts_with("HERMIT_SKID_OVERSHOOT "))
        && !stderr.contains("panicked at")
}

impl ChaosStressOpts {
    /// One `hermit run` of the guest under a fresh chaos seed, its stderr
    /// passed through as it arrives and kept for [`classify`].
    fn run_hermit(&self, common_args: &CommonOpts) -> anyhow::Result<RunOutcome> {
        let mut rng = rand::rng();
        let random_seed: u16 = rng.random();
        let hermit_cli_args = Hermit::new()
            .run(self.guest_program.clone(), self.args.clone())
            .hermit_args(self.hermit_arg.clone())
            .seed(i32::from(random_seed))
            .log_level(tracing::Level::WARN)
            .into_args();

        let mut command = std::process::Command::new(&common_args.hermit_bin);
        command.args(hermit_cli_args).stderr(Stdio::piped());
        println!("   {}", display_cmd(&command).white().bold());
        let started = Instant::now();
        let mut child = command.spawn()?;
        let mut stderr = String::new();
        let mut reader = std::io::BufReader::new(child.stderr.take().expect("stderr is piped"));
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line)? > 0 {
            std::io::stderr().write_all(&line)?;
            stderr.push_str(&String::from_utf8_lossy(&line));
            line.clear();
        }
        let status = child.wait()?;
        let status_text = match status.code() {
            Some(code) => code.to_string(),
            None => status.to_string(),
        };
        println!(
            "Executed command with status: {status_text} in {:.1?}",
            started.elapsed()
        );
        classify(status, &stderr)
    }

    pub fn run(&self, common_args: &CommonOpts) -> anyhow::Result<bool> {
        println!("Run at max {} iterations", self.max_iterations_count);
        let mut success_count = 0;
        let mut fail_count = 0;
        let mut skid_retries = 0;

        for i in 1..=self.max_iterations_count {
            println!("{}", format!(":: Run #{}...", i).yellow().bold());
            let mut attempt = 1;
            let outcome = loop {
                match self.run_hermit(common_args)? {
                    RunOutcome::SkidRefused(refusal) if attempt < SKID_ATTEMPTS => {
                        eprintln!("SKID-RETRY run #{i} attempt {attempt}: {refusal}");
                        skid_retries += 1;
                        attempt += 1;
                    }
                    RunOutcome::SkidRefused(refusal) => anyhow::bail!(
                        "Hermit refused run #{i} for PMU skid overshoot on all {SKID_ATTEMPTS} attempts: {refusal}"
                    ),
                    outcome => break outcome,
                }
            };
            match outcome {
                RunOutcome::Passed => success_count += 1,
                RunOutcome::Failed => fail_count += 1,
                RunOutcome::SkidRefused(_) => unreachable!("retried above"),
            }

            if success_count > 0 && fail_count > 0 {
                println!("Found a passing and a failing run; {skid_retries} skid refusals retried");
                return Ok(true);
            }
        }

        println!(
            "{success_count} passing and {fail_count} failing runs; {skid_retries} skid refusals retried"
        );
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::*;

    const SKID_REFUSAL: &str = "HERMIT_SKID_OVERSHOOT rcb_actual=240935 rcb_target=240682 skid_margin=1000 overshoot=253\n\
        HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=1\n";

    fn exited(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn the_guest_status_decides_a_run() {
        assert_eq!(classify(exited(0), "").unwrap(), RunOutcome::Passed);
        assert_eq!(classify(exited(1), "").unwrap(), RunOutcome::Failed);
        // SIGABRT, raised on Hermit for the guest's own abort.
        assert_eq!(
            classify(ExitStatus::from_raw(6), "").unwrap(),
            RunOutcome::Failed
        );
        // A guest may exit 122 or 125 itself, and may print a marker under
        // another status.
        assert_eq!(classify(exited(122), "").unwrap(), RunOutcome::Failed);
        assert_eq!(
            classify(exited(1), "HERMIT_INTERNAL_FAILURE class=cli-error\n").unwrap(),
            RunOutcome::Failed
        );
    }

    #[test]
    fn a_skid_refusal_is_retried() {
        assert_eq!(
            classify(exited(122), SKID_REFUSAL).unwrap(),
            RunOutcome::SkidRefused(
                "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=1".into()
            )
        );
        // Without Hermit's overshoot report, or beside a panic, it is a refusal
        // like any other.
        let unreported =
            "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=1\n";
        assert!(classify(exited(122), unreported).is_err());
        let panicked = format!("thread 'main' panicked at x.rs:1:1\n{SKID_REFUSAL}");
        assert!(classify(exited(122), &panicked).is_err());
    }

    #[test]
    fn hermit_s_own_outcomes_end_the_search() {
        for (code, stderr) in [
            (122, "HERMIT_POLICY_REFUSAL class=policy-refusal\n"),
            (123, "HERMIT_LOG_CAP class=log-cap\n"),
            (124, "HERMIT_RUN_TIMEOUT class=run-timeout\n"),
            (125, "HERMIT_INTERNAL_FAILURE class=container-child-panic\n"),
        ] {
            let error = classify(exited(code), stderr).unwrap_err().to_string();
            assert!(error.contains(stderr.trim_end()), "{error}");
        }
    }
}
