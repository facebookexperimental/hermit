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
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use std::time::Instant;

use clap::Parser;
use colored::Colorize;
use detcore_model::HERMIT_GUEST_DEADLOCK_EXIT;
use detcore_model::HERMIT_MODEL_DEADLOCK_LINE;
use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
use rand::RngExt as _;

use crate::CommonOpts;
use crate::cli_wrapper::display_cmd;
use crate::cli_wrapper::*;

/// Attempts at one iteration while Hermit refuses its runs for PMU skid
/// overshoot.
const SKID_ATTEMPTS: u32 = detcore_model::SKID_RETRY_MAX_ATTEMPTS;

/// The prefix of the line Hermit prints when it refuses a run for PMU skid
/// overshoot (`classify_failure` in hermit-cli/src/bin/hermit/main.rs).
const SKID_OVERSHOOT_REFUSAL_PREFIX: &str =
    "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=";

/// Hermit's own outcomes that no guest produced, each the exit status from the
/// allocation table in hermit-cli/src/lib.rs with the stderr marker that tells
/// it apart from a guest choosing the same status. A run that ends in one of
/// these says nothing certain about the guest. That includes a deadlock under
/// Hermit's model: no wake Detcore models can reach the guest, which is a
/// native hang only when that model is complete for the program. It includes
/// a `--virtual-timeout` stop too (`class=virtual-timeout`, the status of
/// `--timeout`'s `class=run-timeout`): the bound is Hermit's, the guest had
/// not finished, and a gap in the model (a missed wake turned into a spin-poll)
/// can be what ran the clock past it, so it ends the search as a deadlock does
/// instead of counting as the guest failing under that schedule.
const HERMIT_OUTCOMES: [(i32, &str); 7] = [
    (HERMIT_GUEST_DEADLOCK_EXIT, HERMIT_MODEL_DEADLOCK_LINE),
    (HERMIT_POLICY_REFUSAL_EXIT, "HERMIT_POLICY_REFUSAL "),
    (123, "HERMIT_LOG_CAP "),
    (124, "HERMIT_RUN_TIMEOUT class=run-timeout"),
    (124, "HERMIT_RUN_TIMEOUT class=virtual-timeout "),
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
    /// Iterations to run concurrently. Each iteration is an independent
    /// Hermit process with its own fresh chaos seed; the search stops
    /// starting new iterations once both a passing and a failing run are
    /// found, after the iterations already in flight finish.
    #[clap(long, default_value = "1")]
    jobs: usize,
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
    /// passed through as it arrives and kept for [`classify`]. Every line
    /// this run prints is tagged with its iteration and seed, so a
    /// failure under `--jobs` is attributable and its run reproducible.
    /// The guest's stdout is piped and re-emitted tagged in both modes,
    /// so a sequential and a parallel search present the same output
    /// shape (under a test runner that stdout is captured either way).
    fn run_hermit(&self, common_args: &CommonOpts, run: i32) -> anyhow::Result<(RunOutcome, u16)> {
        let mut rng = rand::rng();
        let random_seed: u16 = rng.random();
        let tag = format!("[run #{run} seed={random_seed}]");
        let hermit_cli_args = Hermit::new()
            .run(self.guest_program.clone(), self.args.clone())
            .hermit_args(self.hermit_arg.clone())
            .seed(i32::from(random_seed))
            .log_level(tracing::Level::WARN)
            .into_args();

        let mut command = std::process::Command::new(&common_args.hermit_bin);
        command
            .args(hermit_cli_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        println!("   {tag} {}", display_cmd(&command).white().bold());
        let started = Instant::now();
        let mut child = command.spawn()?;
        let stdout_tag = tag.clone();
        let stdout_forwarder = child.stdout.take().map(|mut stdout| {
            std::thread::spawn(move || -> std::io::Result<()> {
                let mut reader = std::io::BufReader::new(&mut stdout);
                let mut line = Vec::new();
                while reader.read_until(b'\n', &mut line)? > 0 {
                    let mut out = std::io::stdout().lock();
                    out.write_all(stdout_tag.as_bytes())?;
                    out.write_all(b" ")?;
                    out.write_all(&line)?;
                    line.clear();
                }
                Ok(())
            })
        });
        let mut stderr = String::new();
        let mut reader = std::io::BufReader::new(child.stderr.take().expect("stderr is piped"));
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line)? > 0 {
            let mut out = std::io::stderr().lock();
            out.write_all(tag.as_bytes())?;
            out.write_all(b" ")?;
            out.write_all(&line)?;
            stderr.push_str(&String::from_utf8_lossy(&line));
            line.clear();
        }
        let status = child.wait()?;
        if let Some(forwarder) = stdout_forwarder {
            forwarder.join().expect("stdout forwarder panicked")?;
        }
        let status_text = match status.code() {
            Some(code) => code.to_string(),
            None => status.to_string(),
        };
        println!(
            "{tag} Executed command with status: {status_text} in {:.1?}",
            started.elapsed()
        );
        let outcome = classify(status, &stderr).map_err(|err| anyhow::anyhow!("{tag} {err}"))?;
        Ok((outcome, random_seed))
    }

    /// One search iteration: its guest outcome and how many skid
    /// refusals were retried on the way to it.
    fn run_iteration(&self, common_args: &CommonOpts, i: i32) -> anyhow::Result<(RunOutcome, u32)> {
        println!("{}", format!(":: Run #{}...", i).yellow().bold());
        let mut attempt = 1;
        let mut skid_retries = 0;
        let outcome = loop {
            match self.run_hermit(common_args, i)? {
                (RunOutcome::SkidRefused(refusal), seed) if attempt < SKID_ATTEMPTS => {
                    eprintln!("[run #{i} seed={seed}] SKID-RETRY attempt {attempt}: {refusal}");
                    skid_retries += 1;
                    attempt += 1;
                }
                (RunOutcome::SkidRefused(refusal), seed) => anyhow::bail!(
                    "Hermit refused run #{i} seed={seed} for PMU skid overshoot on all {SKID_ATTEMPTS} attempts; verdict {} (not a test failure): {refusal}",
                    detcore_model::SKID_INFRA_VERDICT
                ),
                (outcome, _seed) => break outcome,
            }
        };
        Ok((outcome, skid_retries))
    }

    pub fn run(&self, common_args: &CommonOpts) -> anyhow::Result<bool> {
        if self.jobs == 0 {
            anyhow::bail!("--jobs must be at least 1");
        }
        println!("Run at max {} iterations", self.max_iterations_count);
        if self.jobs > 1 && self.max_iterations_count > 1 {
            return self.run_parallel(common_args);
        }
        let mut success_count = 0;
        let mut fail_count = 0;
        let mut skid_retries = 0;

        for i in 1..=self.max_iterations_count {
            let (outcome, retries) = self.run_iteration(common_args, i)?;
            skid_retries += retries;
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

    /// The search with `--jobs` iterations in flight. Iteration numbers
    /// are handed out in order; each still draws its own fresh chaos
    /// seed, as sequentially. No new iteration starts once both outcomes
    /// are found or one iteration errors, but iterations already in
    /// flight finish. The verdict is then decided in iteration order
    /// ([`ordered_verdict`]), so it cannot depend on which worker
    /// happened to finish first.
    fn run_parallel(&self, common_args: &CommonOpts) -> anyhow::Result<bool> {
        use std::sync::Mutex;
        use std::sync::atomic::AtomicUsize;

        let workers = self.jobs.min(self.max_iterations_count as usize);
        let results = Mutex::new(
            (0..self.max_iterations_count)
                .map(|_| None::<anyhow::Result<(RunOutcome, u32)>>)
                .collect::<Vec<_>>(),
        );
        let handout = Handout::new();
        let success_count = AtomicUsize::new(0);
        let fail_count = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let Some(i) = handout.take(self.max_iterations_count) else {
                            return;
                        };
                        let result = self.run_iteration(common_args, i);
                        match &result {
                            Ok((outcome, _retries)) => {
                                let (successes, failures) = match outcome {
                                    RunOutcome::Passed => (
                                        success_count.fetch_add(1, Ordering::Relaxed) + 1,
                                        fail_count.load(Ordering::Relaxed),
                                    ),
                                    RunOutcome::Failed => (
                                        success_count.load(Ordering::Relaxed),
                                        fail_count.fetch_add(1, Ordering::Relaxed) + 1,
                                    ),
                                    RunOutcome::SkidRefused(_) => {
                                        unreachable!("retried in run_iteration")
                                    }
                                };
                                if successes > 0 && failures > 0 {
                                    handout.stop();
                                }
                            }
                            Err(err) => {
                                // Printed even when an earlier-indexed pair
                                // will decide the verdict over this error.
                                eprintln!("[run #{i}] error: {err:#}");
                                handout.stop();
                            }
                        }
                        results.lock().unwrap()[(i - 1) as usize] = Some(result);
                    }
                });
            }
        });
        let (found, successes, failures, skid_retries) =
            ordered_verdict(results.into_inner().unwrap())?;
        if found {
            println!("Found a passing and a failing run; {skid_retries} skid refusals retried");
            return Ok(true);
        }
        println!(
            "{successes} passing and {failures} failing runs; {skid_retries} skid refusals retried"
        );
        Ok(false)
    }
}

/// Hands out iteration numbers to the parallel workers in order, and
/// stops handing them out once [`Handout::stop`] is called. Because a
/// number is handed out only to a worker about to run it, at most one
/// iteration per worker can be in flight past the stop: the number of
/// iterations started beyond any deciding point is bounded by the
/// worker count.
struct Handout {
    next: AtomicI32,
    stop: AtomicBool,
}

impl Handout {
    fn new() -> Self {
        Self {
            next: AtomicI32::new(1),
            stop: AtomicBool::new(false),
        }
    }

    fn take(&self, max_iterations_count: i32) -> Option<i32> {
        if self.stop.load(Ordering::Relaxed) {
            return None;
        }
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        (i <= max_iterations_count).then_some(i)
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The parallel search's verdict, decided in iteration order: scan the
/// started iterations from #1, counting outcomes in that order. The
/// search succeeds at the first index by which both a passing and a
/// failing run have been seen; an error before that index ends the
/// search with that error, and an error after it does not change the
/// verdict. Entries past the first not-started iteration are absent,
/// as sequentially. Returns (found, passes, failures, skid retries)
/// counted up to the deciding iteration.
fn ordered_verdict(
    results: Vec<Option<anyhow::Result<(RunOutcome, u32)>>>,
) -> anyhow::Result<(bool, usize, usize, u32)> {
    let mut successes = 0;
    let mut failures = 0;
    let mut skid_retries = 0;
    for entry in results {
        let Some(result) = entry else { break };
        let (outcome, retries) = result?;
        skid_retries += retries;
        match outcome {
            RunOutcome::Passed => successes += 1,
            RunOutcome::Failed => failures += 1,
            RunOutcome::SkidRefused(_) => unreachable!("retried in run_iteration"),
        }
        if successes > 0 && failures > 0 {
            return Ok((true, successes, failures, skid_retries));
        }
    }
    Ok((false, successes, failures, skid_retries))
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
    fn the_handout_stops_handing_out_iterations() {
        let handout = Handout::new();
        assert_eq!(handout.take(3), Some(1));
        assert_eq!(handout.take(3), Some(2));
        handout.stop();
        // Nothing further is handed out once stopped, however many
        // workers ask: at most the already-taken iterations (one per
        // worker) can still be in flight past the stop.
        assert_eq!(handout.take(3), None);
        assert_eq!(handout.take(3), None);
        let handout = Handout::new();
        assert_eq!(handout.take(2), Some(1));
        assert_eq!(handout.take(2), Some(2));
        assert_eq!(handout.take(2), None);
    }

    #[test]
    fn the_parallel_verdict_is_decided_in_iteration_order() {
        let passed = || Some(Ok((RunOutcome::Passed, 0)));
        let failed = || Some(Ok((RunOutcome::Failed, 0)));
        let errored = || Some(Err(anyhow::anyhow!("Hermit itself failed")));
        // Whichever worker finished first at runtime, the indexed
        // results decide identically: an error past the deciding pair
        // does not override it, and one before it ends the search.
        assert_eq!(
            ordered_verdict(vec![passed(), failed(), errored(), None]).unwrap(),
            (true, 1, 1, 0)
        );
        assert!(
            ordered_verdict(vec![passed(), errored(), failed(), None]).is_err(),
            "an error before both outcomes are seen must end the search"
        );
        assert_eq!(
            ordered_verdict(vec![failed(), passed(), None]).unwrap(),
            (true, 1, 1, 0)
        );
        assert_eq!(
            ordered_verdict(vec![passed(), passed(), None]).unwrap(),
            (false, 2, 0, 0)
        );
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
        // A deadlock under Hermit's model is not counted as the guest
        // failing: Hermit's model, not the guest, decided it. A guest that
        // exits 121 itself, with no class line, still fails.
        let deadlock = classify(
            exited(detcore_model::HERMIT_GUEST_DEADLOCK_EXIT),
            "Deadlock detected: thread(s) waiting on futex, but no runnable threads left.\n\
             HERMIT_MODEL_DEADLOCK class=deadlock-under-hermit-model\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            deadlock.contains("HERMIT_MODEL_DEADLOCK class=deadlock-under-hermit-model"),
            "{deadlock}"
        );
        assert_eq!(
            classify(exited(detcore_model::HERMIT_GUEST_DEADLOCK_EXIT), "").unwrap(),
            RunOutcome::Failed
        );
        // Nor is a schedule that ran the guest past --virtual-timeout: the
        // bound is Hermit's, and the guest had not finished. A guest that
        // exits 124 itself, with no class line, still fails.
        let virtual_timeout = classify(
            exited(124),
            "Virtual timeout: at scheduler turn 15, the run's virtual clock is 4004335665 ns \
             past its start, beyond --virtual-timeout=3500000000; no further turn is \
             granted.\nHERMIT_RUN_TIMEOUT class=virtual-timeout bound_ns=3500000000\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            virtual_timeout
                .contains("HERMIT_RUN_TIMEOUT class=virtual-timeout bound_ns=3500000000"),
            "{virtual_timeout}"
        );
        assert_eq!(classify(exited(124), "").unwrap(), RunOutcome::Failed);
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
            (124, "HERMIT_RUN_TIMEOUT class=virtual-timeout bound_ns=1\n"),
            (125, "HERMIT_INTERNAL_FAILURE class=container-child-panic\n"),
        ] {
            let error = classify(exited(code), stderr).unwrap_err().to_string();
            assert!(error.contains(stderr.trim_end()), "{error}");
        }
    }
}
