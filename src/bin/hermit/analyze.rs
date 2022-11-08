// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! A mode for analyzing a hermit run.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::bail;
use clap::Parser;
use colored::Colorize;
use detcore::preemptions::read_trace;
use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use detcore::types::LogicalTime;
use detcore::types::SchedEvent;
use detcore::util::truncated;
use detcore::DetTid;
use detcore::Priority;
use detcore::FIRST_PRIORITY;
use detcore::LAST_PRIORITY;
use hermit::process::Bind;
use hermit::Error;
use rand::Rng;
use rand::SeedableRng;
use rand_pcg::Pcg64Mcg;
use regex::Regex;
use reverie::process::ExitStatus;
use reverie::process::Output;

use super::global_opts::GlobalOpts;
use crate::logdiff::LogDiffCLIOpts;
use crate::run::RunOpts;
use crate::schedule_search::search_for_critical_schedule;
use crate::schedule_search::CriticalSchedule;

#[derive(Debug, Parser)]
pub struct AnalyzeOpts {
    /// Analyze runs that have collected stdout output matching this regular expression.
    #[clap(long, value_name = "REGEX")]
    filter_stdout: Option<Regex>,

    /// Analyze runs that have collected stderr output matching this regular expression.
    #[clap(long, value_name = "REGEX")]
    filter_stderr: Option<Regex>,

    /// Analyze runs that have the specified exit code.  Accepts "nonzero" for all nonzero exit codes.
    /// Accepts "none" or "any" for no filter at all (accepts any exit code).
    /// The default is "nonzero".
    #[clap(long, default_value = "nonzero")]
    filter_exit_code: ExitStatusConstraint,

    /// Insist on perfect determinism before proceeding with the analysis.
    #[clap(long)]
    selfcheck: bool,

    /// If the first run doesn't match the desired criteria, search for one that does.
    #[clap(long)]
    search: bool,

    /// Given a passing/failing run pair, first minimize the chaos-mode interventions necessary to
    /// flip between the two outcomes.  This may accelerate the subsequent binary search.
    #[clap(long)]
    minimize: bool,

    /// Use `--imprecise-timers` during the search phase. Only has an effect if search is enabled.
    #[clap(long)]
    imprecise_search: bool,

    /// Instead of starting with a command (including a `--sched-seed`), start with a recorded preemption file.
    #[clap(long)]
    starting_preemptions: Option<PathBuf>,

    /// Use to seed the PRNG that supplies randomness to the analyzer search process.
    /// If unset, then system randomness is used.
    #[clap(long)]
    seed: Option<u64>,

    /// Print quite a bit of extra information so that you can see exactly what is happening.
    #[clap(long, short)]
    verbose: bool,

    /// A full set of CLI arguments for the original `hermit run` to analyze.
    #[clap(value_name = "ARGS")]
    run_args: Vec<String>,
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

/// Sanity check that a series of preemptions don't include duplicates and are monotonically increasing.
fn sanity_preempts(vec: &[(LogicalTime, Priority)]) {
    let mut set = BTreeSet::new();

    let mut time_last = None;
    for (ns, prio) in vec {
        if let Some(last) = time_last {
            if *ns <= last {
                panic!(
                    "Timestamps failed to monotonically increase ({}), in series:\n {:?}",
                    ns, vec
                );
            }
        }
        assert!(*prio >= FIRST_PRIORITY);
        assert!(*prio <= LAST_PRIORITY);
        time_last = Some(*ns);
        if !set.insert(ns) {
            panic!(
                "sanity_preempts: time series of preemptions contained duplicate entries sharing the timestamp {}",
                ns
            );
        }
    }
}

/// Right now we don't want turning on logging for `hermit analyze` itself to ALSO turn on logging
/// for each one of the (many) individual hermit executions it calls.  This could change in the
/// future and instead share the GlobalOpts passed to `main()`.
const NO_LOGGING_PLZ: GlobalOpts = GlobalOpts {
    log: None,
    log_file: None,
};

impl AnalyzeOpts {
    fn print_and_validate_runopts(&self, ro: &mut RunOpts, log_path: &Path) {
        if self.verbose {
            ro.summary = true;
            eprintln!(
                ":: Full Run configuration (logging to {}):\n {:#?}",
                log_path.display(),
                ro
            );
        }
        ro.validate_args();
    }

    /// Launch a chaos run searching for a failing schudule.
    fn launch_search(
        &self,
        round: u64,
        tmp_dir: &Path,
        sched_seed: u64,
    ) -> Result<Option<PathBuf>, Error> {
        eprintln!(
            ":: {}",
            format!(
                "Searching (round {}) for a failing execution, chaos --sched-seed={} ",
                round, sched_seed
            )
            .yellow()
            .bold()
        );
        let preempts_path =
            tmp_dir.join(format!("search_round_{:0wide$}.preempts", round, wide = 3));
        let mut run_cmd: Vec<String> = vec!["hermit-run".to_string()];
        for arg in &self.run_args {
            run_cmd.push(arg.to_string());
        }

        // TODO: fix duplication by caching this in a new struct that represents the search process.
        let mut ro = RunOpts::from_iter(run_cmd.iter());

        ro.det_opts.det_config.sched_seed = Some(sched_seed);
        ro.det_opts.det_config.sequentialize_threads = true;
        ro.det_opts.det_config.record_preemptions = true;
        ro.det_opts.det_config.record_preemptions_to = Some(preempts_path.clone());
        if self.imprecise_search {
            ro.det_opts.det_config.imprecise_timers = true; // TODO: enable this by default when bugs are fixed.
        }

        let bind_dir: Bind = Bind::from_str(tmp_dir.to_str().unwrap())?;
        ro.bind = vec![bind_dir];

        let log_path = tmp_dir.join(format!("search_round_{:0wide$}.log", round, wide = 3));
        self.print_and_validate_runopts(&mut ro, &log_path);

        // TODO: don't use verify here because we don't want the log.  But if using "run" we need a way to capture the stdout/stderr
        // let out1: Output = ro.run(global)?;
        let log_file = File::create(log_path).unwrap();
        let out1: Output = ro.run_verify(log_file, &NO_LOGGING_PLZ)?;

        if self.output_matches(&out1) {
            Ok(Some(preempts_path))
        } else {
            Ok(None)
        }
    }

    /// Launch a single run with logging and preemption recording.  Return true if it matches the criteria.
    fn launch_logged(
        &self,
        msg: &str,
        log_path: &Path,
        preempts_path: &Path,
    ) -> Result<bool, Error> {
        eprintln!(
            ":: {}",
            format!("{} record preemptions and logs...", msg)
                .yellow()
                .bold()
        );
        let mut run_cmd: Vec<String> = vec!["hermit-run".to_string()];
        for arg in &self.run_args {
            run_cmd.push(arg.to_string());
        }
        let mut ro = RunOpts::from_iter(run_cmd.iter());

        ro.det_opts.det_config.sequentialize_threads = true;
        ro.det_opts.det_config.record_preemptions = true;
        ro.det_opts.det_config.record_preemptions_to = Some(preempts_path.to_path_buf());

        let mut tmp_dir = preempts_path.to_path_buf();
        assert!(tmp_dir.pop());
        let bind_dir: Bind = Bind::from_str(tmp_dir.to_str().unwrap())?;
        ro.bind = vec![bind_dir];

        self.print_and_validate_runopts(&mut ro, log_path);

        let log_file = File::create(log_path)?;
        let out1: Output = ro.run_verify(log_file, &NO_LOGGING_PLZ)?;

        File::create(preempts_path.with_extension("stdout"))
            .unwrap()
            .write_all(&out1.stdout)
            .unwrap();
        File::create(preempts_path.with_extension("stderr"))
            .unwrap()
            .write_all(&out1.stderr)
            .unwrap();

        let is_a_match = self.output_matches(&out1);
        Ok(is_a_match)
    }

    /// Launch a run with preempts provided (to replay). No logging. Return true
    /// if it matches the criteria. If provided, additionally record preemptions
    /// from the run to `record_preempts_path`. This can be used to rerecord with
    /// full global sched events when replaying from a per-thread preemption
    /// record.
    fn launch_controlled(
        &self,
        verify_log_file_path: &Path,
        preempts_path: &Path,
        record_preempts_path: Option<&Path>,
    ) -> Result<bool, Error> {
        let mut run_cmd: Vec<String> = vec!["hermit-run".to_string()];
        for arg in &self.run_args {
            run_cmd.push(arg.to_string());
        }

        // TODO: fix duplication by caching this in a new struct that represents the search process.
        let mut ro = RunOpts::from_iter(run_cmd.iter());

        ro.det_opts.det_config.sequentialize_threads = true;
        ro.det_opts.det_config.replay_preemptions_from = Some(preempts_path.to_path_buf());
        if let Some(path) = record_preempts_path {
            ro.det_opts.det_config.record_preemptions_to = Some(path.to_path_buf());
        }

        let mut tmp_dir = preempts_path.to_path_buf();
        assert!(tmp_dir.pop());
        let bind_dir: Bind = Bind::from_str(tmp_dir.to_str().unwrap())?;
        ro.bind = vec![bind_dir];

        self.print_and_validate_runopts(&mut ro, verify_log_file_path);

        // TODO: don't use verify here because we don't want the log.
        // But if using "run" we need a way to capture the stdout/stderr
        let log_file = File::create(verify_log_file_path).unwrap();
        let out1: Output = ro.run_verify(log_file, &NO_LOGGING_PLZ)?;

        let is_a_match = self.output_matches(&out1);
        Ok(is_a_match)
    }

    /// Runs the program with the specified schedule.
    fn launch_final(
        &self,
        verify_log_file_name: &str,
        schedule_path: &Path,
        critical_event_index: u64,
    ) -> Result<bool, Error> {
        let mut run_cmd: Vec<String> = vec!["hermit-run".to_string()];
        for arg in &self.run_args {
            run_cmd.push(arg.to_string());
        }

        // TODO: fix duplication by caching this in a new struct that represents the search process.
        let mut ro = RunOpts::from_iter(run_cmd.iter());

        ro.det_opts.det_config.sequentialize_threads = true;
        ro.det_opts.det_config.replay_schedule_from = Some(schedule_path.to_path_buf());
        ro.det_opts.det_config.stacktrace_event = [
            (critical_event_index - 1, None),
            (critical_event_index, None),
        ]
        .to_vec();

        let mut tmp_dir = schedule_path.to_path_buf();
        assert!(tmp_dir.pop());
        let bind_dir: Bind = Bind::from_str(tmp_dir.to_str().unwrap())?;
        ro.bind = vec![bind_dir];

        let log_path = tmp_dir.join(verify_log_file_name);
        self.print_and_validate_runopts(&mut ro, &log_path);

        // TODO: don't use verify here because we don't want the log.  But if using "run" we need a way to capture the stdout/stderr
        // let out1: Output = ro.run(global)?;
        let log_file = File::create(log_path).unwrap();
        let out1: Output = ro.run_verify(log_file, &NO_LOGGING_PLZ)?;

        let is_a_match = self.output_matches(&out1);
        Ok(is_a_match)
    }

    /// Runs the program with the specified schedule.
    fn launch_with_schedule(
        &self,
        verify_log_file_name: &str,
        schedule_path: &Path,
    ) -> Result<bool, Error> {
        let mut run_cmd: Vec<String> = vec!["hermit-run".to_string()];
        for arg in &self.run_args {
            run_cmd.push(arg.to_string());
        }

        // TODO: fix duplication by caching this in a new struct that represents the search process.
        let mut ro = RunOpts::from_iter(run_cmd.iter());

        ro.det_opts.det_config.sequentialize_threads = true;
        ro.det_opts.det_config.replay_schedule_from = Some(schedule_path.to_path_buf());

        let mut tmp_dir = schedule_path.to_path_buf();
        assert!(tmp_dir.pop());
        let bind_dir: Bind = Bind::from_str(tmp_dir.to_str().unwrap())?;
        ro.bind = vec![bind_dir];

        let log_path = tmp_dir.join(verify_log_file_name);
        self.print_and_validate_runopts(&mut ro, &log_path);

        // TODO: don't use verify here because we don't want the log.  But if using "run" we need a way to capture the stdout/stderr
        // let out1: Output = ro.run(global)?;
        let log_file = File::create(log_path).unwrap();
        let out1: Output = ro.run_verify(log_file, &NO_LOGGING_PLZ)?;

        let is_a_match = self.output_matches(&out1);
        Ok(is_a_match)
    }

    // TODO: replace this with a more general way to convert RunOpts back to CLI args.
    fn to_repro_cmd(&self, preempts_path: &Path, extra_flags: &str) -> String {
        let mut tmp_dir = preempts_path.to_path_buf();
        assert!(tmp_dir.pop());

        format!(
            "hermit --log-file=/dev/stderr run --bind={} --sequentialize-threads --replay-preemptions-from='{}' {} {}",
            tmp_dir.as_path().to_string_lossy(),
            preempts_path.to_string_lossy(),
            extra_flags,
            self.run_args.join(" "),
        )
    }

    fn to_repro_chaos(&self, seed: u64) -> String {
        let mut str = format!("hermit --log-file=/dev/stderr run --seed={} ", seed);
        str.push_str(&self.run_args.join(" "));
        str
    }

    /// It's weird if no filter is specified.
    fn has_filters(&self) -> bool {
        self.filter_stdout.is_some()
            || self.filter_stderr.is_some()
            || self.filter_exit_code != ExitStatusConstraint::Any
    }

    /// Create our workspace and verify the input run matches the criteria, or find one that does.
    fn phase1_setup_and_search(&mut self) -> Result<(PathBuf, PathBuf, PathBuf), Error> {
        eprintln!(
            ":: {} hermit run {}",
            "Studying execution: ".yellow().bold(),
            self.run_args.join(" ")
        );
        let dir = tempfile::Builder::new()
            .prefix("hermit_analyze")
            .tempdir()?;
        let dir_path = dir.into_path(); // For now always keep the temporary results.
        eprintln!(":: Temp workspace: {}", dir_path.display());

        let run1_log_path = dir_path.join("run1_log");
        let preempts_path = dir_path.join("orig.preempts");
        if let Some(p) = &self.starting_preemptions {
            // Copy into our temp working folder so everything is self contained.
            std::fs::copy(p, &preempts_path).expect("copy file to succeed");
        }

        let is_a_match = if self.starting_preemptions.is_none() {
            // Translate the seed into a set of preemptions we can work from.
            self.launch_logged("Establish failure:", &run1_log_path, &preempts_path)?
        } else {
            true
        };

        if !is_a_match {
            if self.search {
                eprintln!(
                    ":: {}",
                    "First run did not match criteria; now searching for a matching run..."
                        .red()
                        .bold()
                );
                self.do_search(&preempts_path);
            } else {
                panic!(
                    ":: {}",
                    "FAILED. The run did not match the specified criteria. Try --search."
                        .red()
                        .bold()
                );
            }
        } else if self.has_filters() {
            eprintln!(":: {}", "First run matched desired filters.".green().bold());
        } else {
            eprintln!(":: {}", "WARNING: run without any --filter arguments, so accepting ALL runs. This is probably not what you wanted.".red().bold());
        }

        Ok((dir_path, run1_log_path, preempts_path))
    }

    /// Reduce the set of preemptions needed to match the criteria.
    ///
    /// Takes the input (non-minimized) preemptions as a file path and returns the minimized
    /// preemptions as a data structure in memory.
    ///
    /// # Returns
    /// - Minimized preemption record (in memory),
    /// - Path of a file containing that same minimized preemption record,
    /// - Path of the log file that corresponds to the last matching (minimal) run, IF minimized.
    fn phase2_minimize(
        &mut self,
        global: &GlobalOpts,
        preempts_path: &Path,
    ) -> (PreemptionRecord, PathBuf, Option<PathBuf>) {
        if self.minimize {
            // In this scenario we need to work with preemptions.
            let (min_pr, min_pr_path, min_log_path) = self.minimize(preempts_path, global);
            eprintln!(
                ":: {}\n {}",
                "Successfully minimized to these critical interventions:"
                    .green()
                    .bold(),
                truncated(1000, serde_json::to_string(&min_pr).unwrap())
            );

            (min_pr, min_pr_path, Some(min_log_path))
        } else {
            // In this scenario we only care about event traces, and never realyl need to work with
            // preemptions.  Still, we'll need to do another run to record the trace.
            let loaded = PreemptionReader::new(preempts_path).load_all();
            (loaded, preempts_path.to_path_buf(), None)
        }
    }

    fn log_diff(
        &self,
        global: &GlobalOpts,
        run1_log_path: &Path,
        run2_log_path: &Path,
    ) -> ExitStatus {
        if self.verbose {
            eprintln!(
                ":: {}",
                "[comparing] with log-diff command:".yellow().bold()
            );
            eprintln!(
                "::   hermit log-diff {} {}",
                run1_log_path.display(),
                run2_log_path.display(),
            );
        }
        let ldopts = LogDiffCLIOpts::new(run1_log_path, run2_log_path);
        ldopts.main(global)
    }

    pub fn phase3_strict_determinism_check(
        &mut self,
        global: &GlobalOpts,
        dir_path: &Path,
        run1_log_path: &Path,
    ) -> Result<(), Error> {
        if self.selfcheck {
            eprintln!(
                ":: {}",
                "[selfcheck] Verifying full determinism before proceeding"
                    .yellow()
                    .bold()
            );
            let run2_log_path = dir_path.join("run2_log");
            let second_matches = self.launch_logged(
                "Second run: ",
                &run2_log_path,
                &dir_path.join("second.preempts"),
            )?;
            let status = self.log_diff(global, run1_log_path, &run2_log_path);
            if !second_matches {
                bail!("First run matched criteria but second run did not.");
            }
            if !status.success() {
                bail!("Log differences found, aborting because of --strict!")
            }
            eprintln!(
                ":: {}",
                "Full determinism verified between run1/run2."
                    .green()
                    .bold()
            );
        }
        Ok(())
    }

    /// Once we have the target MATCHING run in hand (usually crashing/failing),
    /// we need to determine which baseline, non-matching run to use.
    pub fn phase4_choose_baseline_sched_events(
        &mut self,
        global: &GlobalOpts,
        matching_pr: &PreemptionRecord,
        dir_path: &Path,
    ) -> PathBuf {
        let path = dir_path.join("nearby_non_matching.events");

        if self.minimize {
            // If we're minimizing, then we know that ALL interventions in the schedule are critical.
            // Thus omitting any of them is sufficient to exit the target schedule space.
            // Omitting the last one should yield the lowest distance match/non-match schedule pair.
            self.save_nearby_non_matching_sched_events(matching_pr, &path, global);
        } else {
            let empty_pr = matching_pr.clone().strip_contents();
            self.save_final_pass_sched_events(&empty_pr, &path, global);
        }
        path
    }

    /// Perform the binary search through schedule-space, identifying critical events.
    pub fn phase5_bisect_traces(
        &mut self,
        dir_path: &Path,
        failing: Vec<SchedEvent>,
        passing: Vec<SchedEvent>,
    ) -> CriticalSchedule {
        let mut i = 0;
        let test_fn = |sched: &[SchedEvent]| {
            i += 1;
            let sched_path = dir_path.join(format!("edit_dist_{}.events", i));
            let next_sched = PreemptionRecord::from_sched_events(sched.to_owned());
            next_sched.write_to_disk(&sched_path).unwrap();
            let fail = self
                .launch_with_schedule(&format!("edit-distance_{}.log", i), &sched_path)
                .expect("Run failure");
            if fail {
                eprintln!(" => Fail.");
            } else {
                eprintln!(" => Pass.");
            }
            (!fail, sched.to_owned())
        };

        let crit = search_for_critical_schedule(test_fn, passing, failing);
        eprintln!(
            "Critical event of final failing schedule is {}",
            crit.critical_event_index
        );
        crit
    }

    /// Record the schedules on disk as reproducers and report stack-traces of critical events.
    pub fn phase6_record_outputs(
        &mut self,
        dir_path: PathBuf,
        crit: CriticalSchedule,
    ) -> Result<(), Error> {
        let CriticalSchedule {
            failing_schedule,
            passing_schedule,
            critical_event_index,
        } = crit;

        let final_failing_path = dir_path.join("final_failing_sched");
        {
            let pr = PreemptionRecord::from_sched_events(failing_schedule);
            pr.write_to_disk(&final_failing_path).unwrap();
            eprintln!(
                "Wrote final failing schedule to {}",
                final_failing_path.display()
            );
            let final_passing_path = dir_path.join("final_passing_sched");
            let pr = PreemptionRecord::from_sched_events(passing_schedule);
            pr.write_to_disk(&final_passing_path).unwrap();
            eprintln!(
                "Wrote final passing schedule to {}",
                final_passing_path.display()
            );
        }

        {
            eprintln!("\n:: {}", "Final run to print stack traces:".green().bold());
            eprintln!("These two operations, on different threads, are RACING with eachother.");
            eprintln!(
                "The current order of events {} and {} is causing a FAILURE.",
                critical_event_index - 1,
                critical_event_index
            );
            eprintln!(
                "You must add synchronization to prevent these operations from racing, or give them a different order.\n",
            );

            // "Final run for stack traces:",
            let res = self.launch_final(
                "final_run_log",
                &final_failing_path,
                critical_event_index as u64,
            )?;
            if res {
                eprintln!(":: {}", "Completed analysis successfully.".green().bold());
                Ok(())
            } else {
                bail!("Internal error! Final run did NOT match the criteria as expected!")
            }
        }
    }

    pub fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let (dir_path, run1_log_path, preempts_path) = self.phase1_setup_and_search()?;
        let (min_pr, _min_pr_path, _maybe_min_log) = self.phase2_minimize(global, &preempts_path);
        let normalized_preempts = min_pr.normalize();
        eprintln!(
            ":: {}\n {}",
            "Normalized, that schedule becomes:".green().bold(),
            truncated(
                1000,
                serde_json::to_string_pretty(&normalized_preempts).unwrap()
            )
        );

        self.phase3_strict_determinism_check(global, &dir_path, &run1_log_path)?;

        let final_pr = normalized_preempts;
        let final_sched_events_path = dir_path.join("final_matching.events");
        self.save_final_sched_events(&final_pr, &final_sched_events_path, global);

        let non_matching_sched_events_path =
            self.phase4_choose_baseline_sched_events(global, &final_pr, &dir_path);

        let failing = read_trace(&final_sched_events_path);
        let passing = read_trace(&non_matching_sched_events_path);

        let crit_sched = self.phase5_bisect_traces(&dir_path, failing, passing);

        self.phase6_record_outputs(dir_path, crit_sched)?;

        Ok(ExitStatus::SUCCESS)
    }

    fn save_final_pass_sched_events(
        &self,
        final_preempts: &PreemptionRecord,
        sched_events_path: &Path,
        _global: &GlobalOpts,
    ) {
        let mut tmp_dir = sched_events_path.to_path_buf();
        assert!(tmp_dir.pop());

        let final_preempts_path = tmp_dir.join("final_pass.preempts");
        final_preempts
            .write_to_disk(&final_preempts_path)
            .expect("write of preempts file to succeed");

        // Verify that the new preemption record does in fact now cause a matching execution,
        // and rerecord during this verification with full recording that include sched events
        eprintln!(
            ":: {}:",
            "Verify final baseline schedule causes criteria NOT to hold, and record sched events"
                .yellow()
                .bold()
        );
        eprintln!(
            ":: {}",
            self.to_repro_cmd(
                &final_preempts_path,
                &format!(
                    "--record-preemptions-to={}",
                    sched_events_path.to_string_lossy()
                )
            )
        );
        if !self
            .launch_controlled(
                &tmp_dir.join("verify_final_pass.log"),
                &final_preempts_path,
                Some(sched_events_path),
            )
            .unwrap()
        {
            eprintln!("Good: baseline run does not match criteria (i.e. pass not fail).");
        }
    }

    fn save_final_sched_events(
        &self,
        final_preempts: &PreemptionRecord,
        sched_events_path: &Path,
        _global: &GlobalOpts,
    ) {
        let mut tmp_dir = sched_events_path.to_path_buf();
        assert!(tmp_dir.pop());

        let final_preempts_path = tmp_dir.join("final.preempts");
        final_preempts
            .write_to_disk(&final_preempts_path)
            .expect("write of preempts file to succeed");

        // Verify that the new preemption record does in fact now cause a matching execution,
        // and rerecord during this verification with full recording that include sched events
        eprintln!(
            ":: {}:",
            "Verify final preemption record causes criteria to hold and record sched events"
                .yellow()
                .bold()
        );
        eprintln!(
            ":: {}",
            self.to_repro_cmd(
                &final_preempts_path,
                &format!(
                    "--record-preemptions-to={}",
                    sched_events_path.to_string_lossy()
                )
            )
        );
        if !self
            .launch_controlled(
                &tmp_dir.join("verify_final_fail.log"),
                &final_preempts_path,
                Some(sched_events_path),
            )
            .unwrap()
        {
            panic!("Final preemption record still fails criteria");
        }
    }

    fn save_nearby_non_matching_sched_events(
        &self,
        matching_preempts: &PreemptionRecord,
        sched_events_path: &Path,
        _global: &GlobalOpts,
    ) {
        // Given preemptions that hermit analyze has determined are critical to match the criteria
        // (most commonly, a failing execution), removing the last critical preemption should
        // cause the minimal execution change to now no longer match the criteria (most commonly,
        // an execution that now passes).
        let non_matching_preempts = matching_preempts.with_latest_preempt_removed();

        // Validate the preemption record
        if let Err(e) = non_matching_preempts.validate() {
            panic!(
                "Hermit analyzer produced corrupt nearby non-matching preemption record, cannot proceed.\n\
                Error: {}\n\n\
                Corrupt record: {}",
                e, non_matching_preempts,
            );
        }

        let mut tmp_dir = sched_events_path.to_path_buf();
        assert!(tmp_dir.pop());
        let non_matching_preempts_path = tmp_dir.join("nearby_non_matching.preempts");
        non_matching_preempts
            .write_to_disk(&non_matching_preempts_path)
            .expect("write of preempts file to succeed");

        // Verify that the new preemption record does in fact now cause a non-matching execution,
        // and rerecord during this verification with full recording that include sched events
        eprintln!(
            ":: {}:",
            "Verify preemption record without latest critical preempt causes criteria to fail and record sched events"
                .yellow()
                .bold()
        );
        eprintln!(
            ":: {}",
            self.to_repro_cmd(
                &non_matching_preempts_path,
                &format!(
                    "--record-preemptions-to={}",
                    sched_events_path.to_string_lossy()
                )
            )
        );
        if self
            .launch_controlled(
                &tmp_dir.join("verify_nearby_non_matching.log"),
                &non_matching_preempts_path,
                Some(sched_events_path),
            )
            .unwrap()
        {
            panic!("New preemption record still passes criteria");
        }
    }

    /// Iteratively minimize the schedule needed to produce the error.
    ///
    /// # Returns
    /// - Minimized preemption record (in memory),
    /// - Path of a file containing that same minimized preemption record,
    /// - Path of the log file that corresponds to the last matching (minimal) run.
    fn minimize(
        &self,
        preempts_path: &Path,
        _global: &GlobalOpts,
    ) -> (PreemptionRecord, PathBuf, PathBuf) {
        let pr = PreemptionReader::new(preempts_path);
        let mut tmp_dir = preempts_path.to_path_buf();
        assert!(tmp_dir.pop());

        let tids: Vec<DetTid> = pr.all_threads();
        eprintln!(
            ":: {}",
            format!(
                "RootCause: starting with schedule of {} preemptions for {} threads.",
                pr.size(),
                tids.len()
            )
            .yellow()
            .bold()
        );
        let init_schedule: PreemptionRecord = pr.load_all();
        if self.verbose {
            eprintln!(
                "Initial schedule:\n{}",
                truncated(1000, format!("{}", init_schedule))
            );
        }

        let mut round: u64 = 0;

        // All the preemption points we don't know about yet.  Are they critical?
        let mut remaining_unknown: BTreeMap<DetTid, _> = BTreeMap::new();
        for (dtid, history) in init_schedule.extract_all() {
            let vec = history.as_vec();
            sanity_preempts(&vec);
            remaining_unknown.insert(dtid, vec);
        }

        // The ones we know for sure are critical.
        let mut critical_preempts: BTreeMap<DetTid, BTreeSet<(LogicalTime, Priority)>> =
            BTreeMap::new();

        // We take threads out of consideration if they have no interventions:
        let all_threads = pr.all_threads();
        let mut remaining_threads = Vec::new();

        let min_seed = self.seed.unwrap_or_else(|| {
            let mut rng0 = rand::thread_rng();
            let seed: u64 = rng0.gen();
            seed
        });
        eprintln!(
            ":: {}",
            format!("Minimization search using RNG seed {}", min_seed)
                .yellow()
                .bold()
        );
        let mut rng = Pcg64Mcg::seed_from_u64(min_seed);

        // The number of preemptions to remove each round:
        let mut batch_sizes = BTreeMap::new();
        for tid in all_threads {
            let len = remaining_unknown.get(&tid).unwrap().len();
            batch_sizes.insert(tid, std::cmp::max(1, len / 4));
            if len > 0 {
                remaining_threads.push(tid)
            }
        }

        let mut last_attempt: Option<PreemptionRecord> = None;
        let mut last_matching_attempt = None;
        let mut last_matching_pr_file = None;
        let mut last_matching_log = None;

        // Algorithm: the union of remaining_unknown plus critical_preempts represents our
        // current point in the space of schedules to explore.
        loop {
            round += 1;
            {
                let sum: usize = remaining_unknown.iter().map(|(_, v)| v.len()).sum();
                let sum2: usize = critical_preempts.iter().map(|(_, s)| s.len()).sum();
                eprintln!(
                    ":: {}",
                    format!(
                        "Minimizing schedule, round {}, {} interventions plus {} critical found",
                        round, sum, sum2
                    )
                    .yellow()
                    .bold(),
                );
                if self.verbose {
                    eprintln!(":: All critical preemptions: {:?}", critical_preempts);
                }
                if remaining_threads.is_empty() {
                    if sum2 == 0 {
                        eprintln!(
                            ":: {}",
                            "Minimized to ZERO interventions.  The base run matches the critera."
                                .green()
                                .bold()
                        );
                    } else {
                        eprintln!(
                            ":: {}",
                            format!("Minimized to {} interventions.", sum2)
                                .green()
                                .bold(),
                        );
                    }
                    break;
                }
            }

            let selected_ix = rng.gen_range(0..remaining_threads.len());
            let selected_tid = *remaining_threads.get(selected_ix).unwrap();
            let mut cut = {
                let batch = batch_sizes.get_mut(&selected_tid).unwrap();
                assert!(*batch > 0);
                let preempts = remaining_unknown.get_mut(&selected_tid).unwrap();
                assert!(!preempts.is_empty());
                *batch = std::cmp::min(*batch, preempts.len());

                let len = preempts.len();
                let cut = preempts.split_off(len - *batch);
                eprintln!(
                    ":: Shaving {} off of {} preemptions for tid {}: {}",
                    batch,
                    len,
                    selected_tid,
                    if self.verbose {
                        truncated(79, format!("{:?}", cut))
                    } else {
                        "".to_string()
                    }
                );
                if preempts.is_empty() {
                    remaining_threads.swap_remove(selected_ix);
                }
                cut
            };

            {
                let new_preempts_path =
                    tmp_dir.join(format!("round_{:0wide$}.preempts", round, wide = 3));
                let pr_new = {
                    // Expensive... union back in the critical_preempts:
                    let mut btmap = remaining_unknown.clone();
                    btmap.iter_mut().for_each(|(tid, vec)| {
                        if let Some(set) = critical_preempts.get(tid) {
                            for preempt in set {
                                vec.push(*preempt);
                            }
                            // if cfg!(debug)
                            {
                                sanity_preempts(vec);
                            }
                        }
                    });
                    PreemptionRecord::from_vecs(&btmap)
                };
                if let Err(e) = pr_new.validate() {
                    panic!(
                        "Hermit analyzer produced corrupt preemption record, cannot proceed.\n\
                        Error: {}\n\n\
                        Corrupt record: {}\n\n\
                        Last good state: {}",
                        e,
                        pr_new,
                        if let Some(last) = last_attempt {
                            last.to_string()
                        } else {
                            "<none>".to_string()
                        }
                    );
                }
                pr_new
                    .write_to_disk(&new_preempts_path)
                    .expect("write of preempts file to succeed");
                last_attempt = Some(pr_new);

                let batch = batch_sizes.get_mut(&selected_tid).unwrap();
                eprintln!(":: {}", self.to_repro_cmd(&new_preempts_path, ""));
                let log_path = tmp_dir.join(&format!("round_{:0wide$}.log", round, wide = 3));
                if self
                    .launch_controlled(&log_path, &new_preempts_path, None)
                    .unwrap()
                {
                    eprintln!(
                        ":: {}",
                        "New run matches criteria, continuing.".green().bold()
                    );
                    last_matching_attempt = last_attempt.clone();
                    last_matching_pr_file = Some(new_preempts_path.clone());
                    last_matching_log = Some(log_path);
                    *batch += 1;
                    continue;
                } else {
                    eprintln!(
                        ":: {}",
                        "New run fails criteria, backtracking..".red().bold()
                    );
                    if *batch == 1 {
                        let critical = cut.pop().unwrap();
                        assert!(cut.is_empty());
                        eprintln!(
                            ":: Knocked out only one preemption ({:?}), so concluding that one is critical.",
                            critical
                        );
                        let entry = critical_preempts
                            .entry(selected_tid)
                            .or_insert_with(BTreeSet::new);

                        assert!(entry.insert(critical));
                        continue;
                    } else {
                        *batch /= 2;
                        eprintln!(
                            ":: Batch size for minimizing tid {} reduced to {}",
                            selected_tid, batch
                        );
                        // Hacky way to backtrack one step for now:
                        {
                            let preempts = remaining_unknown.get_mut(&selected_tid).unwrap();
                            preempts.append(&mut cut);
                        }
                        continue;
                    }
                }
            }
        }
        (
            last_matching_attempt.expect("at least one run to match the criteria"),
            last_matching_pr_file.expect("at least one run to match the criteria"),
            last_matching_log.expect("at least one run to match the criteria"),
        )
    }

    /// Search for a failing run. Destination passing style: takes the path that it writes its output to.
    fn do_search(&self, preempts_path: &Path) {
        let mut tmp_dir = preempts_path.to_path_buf();
        assert!(tmp_dir.pop());

        let search_seed = self.seed.unwrap_or_else(|| {
            let mut rng0 = rand::thread_rng();
            let seed: u64 = rng0.gen();
            seed
        });
        eprintln!(
            ":: {}",
            format!("Failure search using RNG seed {}", search_seed)
                .yellow()
                .bold()
        );
        let mut rng = Pcg64Mcg::seed_from_u64(search_seed);

        let mut round = 0;
        loop {
            let sched_seed = rng.gen();
            if let Some(preempts) = self
                .launch_search(round, &tmp_dir, sched_seed)
                .unwrap_or_else(|e| panic!("Error: {}", e))
            {
                let init_schedule: PreemptionRecord = PreemptionReader::new(&preempts).load_all();
                if self.verbose {
                    eprintln!(
                        ":: {}:\nSchedule:\n {}",
                        "Search successfully found a failing run:".green().bold(),
                        truncated(1000, serde_json::to_string(&init_schedule).unwrap()),
                    );
                }
                eprintln!(
                    ":: {}:\n{}",
                    "Reproducer".green().bold(),
                    self.to_repro_chaos(sched_seed)
                );
                std::fs::copy(&preempts, &preempts_path).expect("file copy to succeed");
                break;
            }
            round += 1;
        }
    }

    /// Does the run meet the criteria we are looking for (e.g. a particular error message).
    pub fn output_matches(&self, out: &Output) -> bool {
        let mut answer = true;
        if let Some(pat) = &self.filter_stdout {
            let str = String::from_utf8_lossy(&out.stdout);
            if !pat.is_match(&str) {
                if self.verbose {
                    eprintln!("Mismatch for stdout pattern {}", pat);
                    eprintln!("Stdout:\n{}", str);
                }
                answer = false;
            }
        }
        if let Some(pat) = &self.filter_stderr {
            let str = String::from_utf8_lossy(&out.stderr);
            if self.verbose {
                eprintln!("Mismatch for stderr pattern {}", pat);
            }
            if !pat.is_match(&str) {
                answer = false;
            }
        }

        if !self.filter_exit_code.is_match(out.status) {
            if self.verbose {
                eprintln!(
                    "  Exit code {} is not what we're looking for.",
                    out.status.into_raw()
                );
            }
            answer = false;
        }
        answer
    }
}
