/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A mode for analyzing a hermit run.

use std::cmp;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use colored::Colorize;
use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use detcore::types::SchedEvent;
use detcore::util::truncated;
use hermit::Error;
use rand::Rng;
use rand::SeedableRng;
use rand_pcg::Pcg64Mcg;
use reverie::process::ExitStatus;
use reverie::process::Output;
use reverie::PrettyBacktrace;

use crate::analyze::consts::*;
use crate::analyze::rundata::RunData;
use crate::analyze::types::AnalyzeOpts;
use crate::analyze::types::ExitStatusConstraint;
use crate::analyze::types::Report;
use crate::analyze::types::ReportCriticalEvent;
use crate::global_opts::GlobalOpts;
use crate::logdiff::LogDiffCLIOpts;
use crate::run::RunOpts;
use crate::schedule_search::search_for_critical_schedule;
use crate::schedule_search::CriticalSchedule;

/// Compare only preemptions, not recorded schedules.
fn preempt_files_equal(path1: &Path, path2: &Path) -> bool {
    let mut pr1 = PreemptionReader::new(path1).load_all();
    let mut pr2 = PreemptionReader::new(path2).load_all();
    pr1.preemptions_only();
    pr2.preemptions_only();
    pr1 == pr2
}

fn yellow_msg(msg: &str) {
    eprintln!(":: {}", msg.yellow().bold());
}

impl AnalyzeOpts {
    pub fn get_tmp(&self) -> anyhow::Result<&Path> {
        if let Some(pb) = &self.tmp_dir {
            Ok(pb.as_path())
        } else {
            bail!("Expected tmp_dir to be set at this point!")
        }
    }

    /// Launch a chaos run searching for a target (e.g. failing) schudule.
    /// Returns Some if a target schedule is found.
    fn launch_search(&self, round: u64, sched_seed: u64) -> Result<Option<RunData>, Error> {
        yellow_msg(&format!(
            "Searching (round {}) for a target execution, chaos --sched-seed={} ",
            round, sched_seed
        ));
        let runname = format!("search_round_{:0wide$}", round, wide = 3);
        let mut rundat = RunData::new_baseline(self, runname)?.with_preemption_recording();
        rundat.runopts.det_opts.det_config.sched_seed = Some(sched_seed);
        if self.imprecise_search {
            rundat.runopts.det_opts.det_config.imprecise_timers = true; // TODO: enable this by default when bugs are fixed.
        }
        rundat.launch()?;
        if rundat.is_a_match() {
            eprintln!(
                ":: {}:\n    {}",
                "Target run established by --search. Reproducer"
                    .green()
                    .bold(),
                rundat.to_repro(),
            );
            Ok(Some(rundat))
        } else {
            Ok(None)
        }
    }

    // TODO: REMOVE. Only used by minimize atm.
    //
    /// Launch a run with preempts provided (to replay). No logging. Return true if it matches the
    /// criteria. If provided, additionally record full schedule events from the run to
    /// `record_sched_path`.
    pub(super) fn launch_from_preempts_to_sched(
        &self,
        runname: &str,
        preempts_path: &Path,
        record_sched_path: Option<&Path>,
    ) -> anyhow::Result<(bool, RunOpts)> {
        let mut run = RunData::new_baseline(self, runname.to_string())?
            .with_preempts_path_in(preempts_path.to_path_buf());

        if let Some(path) = record_sched_path {
            run = run.with_schedule_recording_to(path.to_path_buf());
        }
        run.launch()?;
        let is_match = run.is_a_match();
        Ok((is_match, run.into_runopts()))
    }

    /// Runs the program with the specified schedule.
    /// Returns whether the final run met the criteria as expected.
    /// Also returns the paths to stack traces of the two critical events.
    fn launch_for_stacktraces(
        &self,
        runname: &str,
        schedule_path: &Path,
        critical_event_index: u64,
    ) -> anyhow::Result<(RunData, PathBuf, PathBuf)> {
        let mut run = RunData::new_baseline(self, runname.to_string())?
            .with_schedule_replay_from(schedule_path.to_path_buf());

        let root_path = run.root_path();
        let stack1_path = root_path.with_extension("stack1");
        let stack2_path = root_path.with_extension("stack2");

        run.runopts.det_opts.det_config.stacktrace_event = [
            (critical_event_index - 1, Some(stack1_path.clone())),
            (critical_event_index, Some(stack2_path.clone())),
        ]
        .to_vec();
        run.launch()?;
        Ok((run, stack1_path, stack2_path))
    }

    /// It's weird if no filter is specified.
    fn has_filters(&self) -> bool {
        self.target_stdout.is_some()
            || self.target_stderr.is_some()
            || self.target_exit_code != ExitStatusConstraint::Any
    }

    /// Is the criteria the most common setting of target = failure = nonzero exit.
    fn is_vanilla_criteria(&self) -> bool {
        self.target_stdout.is_none()
            && self.target_stderr.is_none()
            && self.target_exit_code == ExitStatusConstraint::NonZero
    }

    fn display_criteria(&self) -> String {
        let mut strs: Vec<String> = Vec::new();
        match &self.target_exit_code {
            ExitStatusConstraint::Exact(c) => {
                strs.push(format!("exit code={}", c));
            }
            ExitStatusConstraint::NonZero => {
                strs.push("nonzero exit".to_string());
            }
            ExitStatusConstraint::Any => {}
        }
        if self.target_stdout.is_some() {
            strs.push("matching stdout".to_string());
        }
        if self.target_stderr.is_some() {
            strs.push("matching stderr".to_string());
        }
        strs.join(", ")
    }

    /// Mutates AnalyzeOpts in order to initialize some fields.
    fn phase0_initialize(&mut self) -> anyhow::Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("hermit_analyze")
            .tempdir()?;
        let tmpdir_path = dir.into_path(); // For now always keep the temporary results.
        eprintln!(":: Temp workspace: {}", tmpdir_path.display());
        self.tmp_dir = Some(tmpdir_path);
        Ok(())
    }

    /// Create our workspace and verify the input run matches the criteria, or find one that does.
    /// Returns the results of the target run.
    fn phase1_establish_target_run(&self) -> Result<RunData, Error> {
        let mut run1data = RunData::new_run1_target(self, "run1_target".to_string())?;
        eprintln!(
            ":: {} hermit run {}",
            "Studying target execution: ".yellow().bold(),
            run1data.to_repro()
        );

        if let Some(p) = &self.run1_preemptions {
            let preempts_path = run1data.preempts_path_out();
            // Copy preempts to the output location as though they were recorded by this run:
            std::fs::copy(p, preempts_path).expect("copy file to succeed");
            // Careful, returning a NON-launched RunData just to contain the output path.
            return Ok(run1data);
        }

        // Translate the seed into a set of preemptions we can work from.
        let mut run1data = run1data.with_preemption_recording();
        run1data.launch()?;

        if !run1data.is_a_match() {
            if self.search {
                eprintln!(
                    ":: {}",
                    "First run did not match target criteria; now searching for a matching run..."
                        .red()
                        .bold()
                );
                Ok(self.do_search())
            } else {
                bail!("FAILED. The run did not match the target criteria. Try --search.");
            }
        } else if self.has_filters() {
            eprintln!(
                ":: {}",
                format!(
                    "First run matched target criteria ({}).",
                    self.display_criteria()
                )
                .green()
                .bold(),
            );
            Ok(run1data)
        } else {
            eprintln!(":: {}", "WARNING: run without any --filter arguments, so accepting ALL runs. This is probably not what you wanted.".red().bold());
            Ok(run1data)
        }
    }

    /// Reduce the set of preemptions needed to match the criteria.
    ///
    /// Takes the input (non-minimized) preemptions as a file path and returns the minimized
    /// preemptions as a data structure in memory.
    ///
    /// # Returns
    /// - The on-target run with minimized schedule.
    fn phase2_minimize(
        &self,
        global: &GlobalOpts,
        mut last_run: RunData,
    ) -> anyhow::Result<RunData> {
        if self.minimize {
            // In this scenario we need to work with preemptions.

            let preempts_path = last_run.preempts_path_out();
            let (min_pr, min_pr_path, min_log_path) = self.minimize(preempts_path, global)?;
            eprintln!(
                ":: {}\n {}",
                "Successfully minimized to these critical interventions:"
                    .green()
                    .bold(),
                truncated(1000, serde_json::to_string(&min_pr).unwrap())
            );

            // TEMP: construct a RunData post-facto, until we finish the minimize overhaul:
            let runname = "minimized".to_string();
            let min_run = RunData::from_minimize_output(
                self,
                runname,
                last_run.runopts.clone(),
                min_pr,
                min_pr_path,
                min_log_path,
            );
            Ok(min_run)
        } else {
            // In this scenario we only care about event traces, and never really need to work with
            // preemptions.  Still, we'll need to do another run to record the trace.
            Ok(last_run)
        }
    }

    /// A weaker log difference that does not expect certain lines to be conserved in preemption replay.
    fn log_diff_preemption_replay(
        &self,
        global: &GlobalOpts,
        run1_log_path: &Path,
        run2_log_path: &Path,
    ) -> ExitStatus {
        if self.verbose {
            yellow_msg("[comparing] with log-diff command:");
            eprintln!(
                "    hermit log-diff --ignore-lines=CHAOSRAND {} {}",
                run1_log_path.display(),
                run2_log_path.display(),
            );
        }
        let mut ldopts = LogDiffCLIOpts::new(run1_log_path, run2_log_path);
        ldopts.more.ignore_lines = vec!["CHAOSRAND".to_string(), "SCHEDRAND".to_string()];
        ldopts.main(global)
    }

    /// Optionally do an extra run to verify that preemptions replay and yield the exact same
    /// execution.a
    pub fn phase3_strict_preempt_replay_check(
        &self,
        global: &GlobalOpts,
        min_run: &mut RunData,
    ) -> Result<(), Error> {
        let min_log_path = &min_run.log_path().unwrap().to_path_buf();
        let min_preempts_path = min_run.preempts_path_out();

        if self.selfcheck {
            yellow_msg("[selfcheck] Verifying target run preserved under preemption-replay");

            let runname = "run1b_selfcheck";
            let mut run1b = RunData::new_run1_target(self, runname.to_string())?
                .with_preempts_path_in(min_preempts_path.to_path_buf())
                .with_preemption_recording();
            eprintln!("    {}", run1b.to_repro());

            run1b.launch()?;
            yellow_msg("[selfcheck] Comparing output from additional run (run1 vs run1b)");
            let run1b_log_path = run1b.log_path().unwrap();
            let status = self.log_diff_preemption_replay(global, min_log_path, run1b_log_path);

            if !run1b.is_a_match() {
                bail!("First run matched criteria but second run did not.");
            }
            if !status.success() {
                bail!(
                    "Log differences found, aborting because --selfcheck requires perfect reproducibility of the target run!"
                )
            }
            let run1b_preempts_path = run1b.preempts_path_out();
            if !preempt_files_equal(min_preempts_path, run1b_preempts_path) {
                bail!(
                    "The preemptions recorded by the additional run did not match the preemptions replayed (no fixed point): {} vs {}",
                    min_preempts_path.display(),
                    run1b_preempts_path.display(),
                );
            }

            eprintln!(
                ":: {}",
                "Identical executions confirmed between target run and its preemption-based replay."
                    .green()
                    .bold()
            );
        }
        Ok(())
    }

    /// Once we have the target MATCHING run in hand (usually crashing/failing), we need to
    /// determine which baseline, non-matching run to use. Then we need to extract the schedule from
    /// it.
    ///
    /// Returns a path to a file containing recorded schedule events for the baseline run.
    pub fn phase4_choose_baseline_sched_events(
        &self,
        matching_pr: PreemptionRecord,
    ) -> anyhow::Result<RunData> {
        if self.minimize {
            // Enforced by the clap conflicts_with annotations:
            assert!(self.run2_seed.is_none());
            assert!(self.run2_preemptions.is_none());
            assert!(self.run2_schedule.is_none());

            // If we're minimizing, then we know that ALL interventions in the schedule are critical.
            // Thus omitting any of them is sufficient to exit the target schedule space.
            // Omitting the last one should yield the lowest distance match/non-match schedule pair.
            let mut pr = matching_pr;
            let mut ix = 1;
            loop {
                let runname = format!("run2_baseline_try{:03}", ix);
                let (still_matching, mut newrun) =
                    self.save_nearby_non_matching_sched_events(&runname, &pr)?;
                if still_matching {
                    pr = newrun.preempts_out().clone();
                    ix += 1;
                } else {
                    return Ok(newrun);
                }
            }
        } else {
            let runname = "run2_baseline".to_string();
            let mut newrun = RunData::new_baseline(self, runname)?.with_preemption_recording();

            // Tweak the runopts according to several different scenarios:
            let ro = &mut newrun.runopts;
            let from_where;
            if let Some(seed) = self.run2_seed {
                // Replay from seed to record schedule.
                ro.det_opts.det_config.seed = seed;
                from_where = "--run2-seed";
            } else if let Some(path) = &self.run2_preemptions {
                // Replay from preemption record to record schedule.
                ro.det_opts.det_config.replay_preemptions_from = Some(path.clone());
                // TODO: if the file already contains a schedule, we don't need to rerun it.
                // Unless selfcheck is specified.
                from_where = "--run2-preemptions";
            } else if let Some(path) = &self.run2_schedule {
                if self.selfcheck {
                    // TODO: Don't trust the recorded schedule is a baseline, replay and check it.
                }
                let runname = "from_existing_run2_schedule".to_string();
                let fakerun = RunData::from_schedule_trace(self, runname, ro.clone(), path.clone());
                return Ok(fakerun);
            } else {
                from_where = "non-chaos (0 extra preemptions) run";

                // Otherwise we just assume that a *baseline* (non-chaos) run will do the trick.
                ro.det_opts.det_config.chaos = false;
            }
            newrun.launch()?;

            eprintln!(
                ":: Recorded schedule from {} as baseline run ({})",
                from_where,
                newrun
                    .sched_path_out()
                    .file_name()
                    .unwrap()
                    .to_string_lossy(),
            );
            if newrun.is_a_match() {
                bail!(
                    "Expectations not met... baseline run matched target criteria when it should not."
                )
            } else {
                eprintln!("Good: baseline run does not match criteria (e.g. pass not fail).");
                Ok(newrun)
            }
        }
    }

    /// Perform the binary search through schedule-space, identifying critical events.
    pub fn phase5_bisect_traces(
        &self,
        target: &[SchedEvent],
        baseline: &[SchedEvent],
    ) -> anyhow::Result<CriticalSchedule> {
        let mut i = 0;

        let test_fn = |sched: &[SchedEvent]| {
            i += 1;
            let runname = format!("bisect_round_{:0wide$}", i, wide = 3);
            let mut newrun = RunData::new_baseline(self, runname)
                .expect("RunData construction to suceed")
                .with_schedule_replay() // Default input path
                .with_schedule_recording(); // Default output path
            // Prepare the next synthetic schedule on disk:
            let next_sched = PreemptionRecord::from_sched_events(sched.to_owned());
            next_sched.write_to_disk(newrun.sched_path_in()).unwrap();

            if self.verbose {
                eprintln!(
                    ":: {}, repro command:\n    {}",
                    format!("Testing execution during search (#{})", i)
                        .yellow()
                        .bold(),
                    newrun.to_repro(),
                );
            }

            newrun.launch().expect("New run to succeed");
            let is_match = newrun.is_a_match();
            if is_match {
                eprintln!(" => Target condition ({})", self.display_criteria());
            } else {
                eprintln!(" => Baseline condition (usually absence of crash)");
            }

            let sched_out = PreemptionReader::new(newrun.sched_path_out()).load_all();
            (!is_match, sched_out.into_global())
        };

        let target = target.to_vec(); // TODO: have search_for_critical_schedule borrow only.
        let baseline = baseline.to_vec();
        let crit = search_for_critical_schedule(
            test_fn,
            baseline,
            target,
            self.verbose,
            self.run_needleman,
        );
        eprintln!(
            "Critical event of final on-target schedule is {}",
            crit.critical_event_index
        );
        Ok(crit)
    }

    fn generate_context(
        sched: &[SchedEvent],
        critical_event_index: usize,
        window_size: usize,
    ) -> String {
        let mut additional_context = "Execution context for the critical events (*):\n".to_string();
        {
            let start = critical_event_index - 1;
            let start = start - cmp::min(window_size, start);
            let end = cmp::min(critical_event_index + window_size + 1, sched.len());

            for (ix, item) in sched.iter().enumerate().take(end).skip(start) {
                additional_context.push_str(&format!(
                    "  {} {}\n",
                    if ix == critical_event_index || ix == critical_event_index - 1 {
                        "*"
                    } else {
                        " "
                    },
                    item
                ));
            }
        }
        additional_context
    }

    /// Record the schedules on disk as reproducers and report stack-traces of critical events.
    pub fn phase6_record_outputs(&self, crit: CriticalSchedule) -> Result<Report, Error> {
        let tmp_dir = self.get_tmp()?;
        let CriticalSchedule {
            failing_schedule,
            passing_schedule,
            critical_event_index,
        } = crit;

        let runname1 = "final_target_for_stacktraces";
        let runname2 = "final_baseline_for_stacktraces";
        let final_failing_path = tmp_dir.join(runname1).with_extension(SCHED_EXT);
        let final_passing_path = tmp_dir.join(runname2).with_extension(SCHED_EXT);

        let passing_context = Self::generate_context(
            &passing_schedule,
            critical_event_index,
            self.execution_context,
        );
        let failing_context = Self::generate_context(
            &failing_schedule,
            critical_event_index,
            self.execution_context,
        );

        {
            let pr = PreemptionRecord::from_sched_events(failing_schedule);
            pr.write_to_disk(&final_failing_path).unwrap();
            eprintln!(
                "Wrote final on-target ({}) schedule to {}",
                self.display_criteria(),
                final_failing_path.display()
            );
            let pr = PreemptionRecord::from_sched_events(passing_schedule);
            pr.write_to_disk(&final_passing_path).unwrap();
            eprintln!(
                "Wrote final baseline (off-target) schedule to {}",
                final_passing_path.display()
            );
        }

        {
            let mut header = String::new();
            header.push_str(
                "These two operations, on different threads, are RACING with eachother.\n",
            );
            header.push_str(&format!(
                "The order of events (#{} and #{} in the schedule) determines the program outcome.\n",
                critical_event_index - 1,
                critical_event_index,
            ));
            header.push_str(
                "You must add synchronization to prevent these operations from racing, or give them a different order.\n",
            );

            if self.is_vanilla_criteria() {
                header.push_str(
                    "Attached are the stacktraces from a PASSING run, but flipping the order of the two events makes the program FAIL (nonzero exit).\n",
                );
            } else {
                header.push_str(&format!(
                    "Attached are the stacktraces from a baseline run, while flipping the order makes the program exhibit the target criteria ({}).\n",
                    self.display_criteria()
                ));
            }

            let (stack1, stack2) = self.run_and_retrieve_stacks(
                runname2,
                final_passing_path,
                critical_event_index,
                false,
            )?;

            // Also print to the screen:
            println!(
                "\n------------------------------ hermit analyze report ------------------------------"
            );
            println!("{}", header);
            Self::print_stack("stack1", &stack1);
            Self::print_stack("stack2", &stack2);
            println!("\n{}", passing_context);

            let report = Report {
                header,
                additional_context: passing_context,
                baseline_run: true,
                critical_event1: ReportCriticalEvent {
                    event_index: critical_event_index - 1,
                    stack: stack1,
                },
                critical_event2: ReportCriticalEvent {
                    event_index: critical_event_index,
                    stack: stack2,
                },
            };

            if self.verbose {
                eprintln!(
                    "\n:: {}",
                    "[verbose] additional TARGET (failing) run to print those stack traces as well.."
                        .yellow()
                        .bold()
                );
                let (stack1, stack2) = self.run_and_retrieve_stacks(
                    runname1,
                    final_failing_path,
                    critical_event_index,
                    true,
                )?;
                println!(
                    "\n----------------------- stacks from final on-target run -----------------------"
                );
                Self::print_stack("stack1", &stack1);
                Self::print_stack("stack2", &stack2);
                println!("\n{}", failing_context);
            }

            Ok(report)
        }
    }

    fn print_stack(which: &str, m_stack: &Option<PrettyBacktrace>) {
        if let Some(s) = m_stack {
            println!("{}", s);
        } else {
            println!(" ({} not available) ", which);
        }
    }

    fn run_and_retrieve_stacks(
        &self,
        runname: &str,
        final_path: PathBuf,
        critical_event_index: usize,
        expected_match: bool,
    ) -> Result<(Option<PrettyBacktrace>, Option<PrettyBacktrace>), Error> {
        let (rundata, stack1_path, stack2_path) =
            self.launch_for_stacktraces(runname, &final_path, critical_event_index as u64)?;
        let res = rundata.is_a_match();
        eprintln!(
            "\n:: {}",
            "Final run to print stack traces completed.  Repro command:"
                .green()
                .bold()
        );
        eprintln!("{}", rundata.to_repro());

        let stack1 = File::open(&stack1_path)
            .map(|file| serde_json::from_reader(file).unwrap())
            .ok();
        let stack2 = File::open(&stack2_path)
            .map(|file| serde_json::from_reader(file).unwrap())
            .ok();

        if res == expected_match {
            eprintln!(":: {}", "Completed analysis successfully.".green().bold());
            Ok((stack1, stack2))
        } else {
            bail!(
                "Internal error! Final run expected match={}, but observed the opposite!",
                expected_match,
            )
        }
    }

    pub fn main(&mut self, global: &GlobalOpts) -> anyhow::Result<ExitStatus> {
        // Not implemented yet:
        if self.run1_schedule.is_some() {
            todo!()
        }
        if self.run2_schedule.is_some() {
            todo!()
        }

        self.phase0_initialize()?; // Need this early to set tmp_dir.
        {
            let dummy = RunData::new_baseline(self, "dummy".to_string())?;
            if !dummy.runopts.det_opts.det_config.chaos {
                eprintln!(
                    ":: {} You may want to turn it on explicitly, along with a --preemption-timeout that works well for this program.",
                    "WARNING: implicitly activating --chaos.".yellow().bold()
                );
            }
        }

        let run1data = self.phase1_establish_target_run()?;

        let mut min_run = self.phase2_minimize(global, run1data)?;
        self.phase3_strict_preempt_replay_check(global, &mut min_run)?;

        min_run.normalize_preempts_out();
        let normalized_preempts = min_run.preempts_out().clone();
        eprintln!(
            ":: {}\n {}",
            &format!(
                "Normalized, that preemption record ({}) becomes:",
                min_run
                    .preempts_path_out()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
            )
            .green()
            .bold(),
            truncated(
                1000,
                serde_json::to_string_pretty(&normalized_preempts.clone_preemptions_only())
                    .unwrap()
            )
        );

        // One endpoint of the bisection search:
        let mut target_endpoint_run = self.save_final_target_sched_events(min_run)?;

        // The other endpoint of the bisection search:
        // What we thought was the final_pr can change here:
        let mut baseline_endpoint_run =
            self.phase4_choose_baseline_sched_events(normalized_preempts)?;

        let baseline_sched_name = baseline_endpoint_run.sched_out_file_name();
        let target_sched_name = target_endpoint_run.sched_out_file_name();
        let target = target_endpoint_run.sched_out();
        let baseline = baseline_endpoint_run.sched_out();

        eprintln!(
            ":: {} (event lengths {} / {}): {} {}",
            "Beginning bisection using endpoints".yellow().bold(),
            baseline.len(),
            target.len(),
            baseline_sched_name,
            target_sched_name,
        );
        let crit_sched = self.phase5_bisect_traces(target, baseline)?;

        let report = self.phase6_record_outputs(crit_sched)?;
        if let Some(path) = &self.report_file {
            let txt = serde_json::to_string(&report).unwrap();
            std::fs::write(path, txt).expect("Unable to write report file");
            eprintln!(
                ":: {}\n {}",
                "Final analysis report written to:".green().bold(),
                path.display()
            );
        }
        self.success_exit_code
            .map_or(Ok(ExitStatus::SUCCESS), |exit_code| {
                Ok(ExitStatus::Exited(exit_code))
            })
    }

    /// This extra run, to record the schedule, thus converting Preemptions to a full Schedule
    /// would be unnecessary if we recorded that each time as we minimize.
    ///
    /// Argument: the last on-target run that was preemption based.
    fn save_final_target_sched_events(&self, mut last_run: RunData) -> anyhow::Result<RunData> {
        let pr = last_run.preempts_out();
        if pr.contains_schedevents() {
            let path = last_run.preempts_path_out();
            // This run already has sched events recorded... no extra run necessary.
            yellow_msg(&format!(
                "Using the last run as the target endpoint ({} contains sched events)",
                path.file_name().unwrap().to_string_lossy()
            ));
            Ok(last_run)
        } else {
            // Verify that the new preemption record does in fact now cause a matching execution,
            // and rerecord during this verification with full recording that include sched events
            yellow_msg(
                "Verify target endpoint preemption record causes criteria to hold and record sched events",
            );
            let runname = "verify_target_endpoint";
            let mut newrun = RunData::new(self, runname.to_string(), last_run.runopts.clone())
                .with_preempts_path_in(last_run.preempts_path_out().to_path_buf())
                .with_schedule_recording();
            newrun.launch()?;
            eprintln!("    {}", newrun.to_repro());
            if !newrun.is_a_match() {
                bail!("Final preemption record still does not match target criteria");
            }
            Ok(newrun)
        }
    }

    // Returns the record, with one knockout, if it still satisfies the criteria that we want it not to.
    fn save_nearby_non_matching_sched_events(
        &self,
        runname: &str,
        matching_preempts: &PreemptionRecord,
    ) -> anyhow::Result<(bool, RunData)> {
        // Given preemptions that hermit analyze has determined are critical to match the criteria
        // (most commonly, a failing execution), removing the last critical preemption should
        // cause the minimal execution change to now no longer match the criteria (most commonly,
        // an execution that now passes).
        let non_matching_preempts = matching_preempts.with_latest_preempt_removed();

        // Validate the preemption record
        if let Err(e) = non_matching_preempts.validate() {
            bail!(
                "Hermit analyzer produced corrupt nearby non-matching preemption record, cannot proceed.\n\
                Error: {}\n\n\
                Corrupt record: {}",
                e,
                non_matching_preempts,
            );
        }

        let mut newrun = RunData::new_baseline(self, runname.to_string())?
            .with_preempts_in(non_matching_preempts)
            .with_schedule_recording();

        // Verify that the new preemption record does in fact now cause a non-matching execution,
        // and rerecord during this verification with full recording that include sched events
        yellow_msg(
            "Verify preemption record *without* latest critical preempt causes criteria non-match. Also record sched events.",
        );
        newrun.launch()?;
        eprintln!("    {}", newrun.to_repro());
        if newrun.is_a_match() {
            eprintln!(
                "{}",
                ":: New preemption record still matches criteria! Attempting further knockouts.."
                    .red()
                    .bold(),
            );
            Ok((true, newrun))
        } else {
            Ok((false, newrun))
        }
    }

    /// Search for a target run. Return the run when found.
    fn do_search(&self) -> RunData {
        let search_seed = self.analyze_seed.unwrap_or_else(|| {
            let mut rng0 = rand::thread_rng();
            let seed: u64 = rng0.gen();
            yellow_msg(&format!("WARNING: performing --search with system randomness, use --analyze-seed={} to repro.", seed));
            seed
        });
        yellow_msg(&format!("Failure search using RNG seed {}", search_seed));
        let mut rng = Pcg64Mcg::seed_from_u64(search_seed);

        let mut round = 0;
        loop {
            let sched_seed = rng.gen();
            if let Some(mut rundat) = self
                .launch_search(round, sched_seed)
                .unwrap_or_else(|e| panic!("Error: {}", e))
            {
                if self.verbose {
                    let preempts = rundat.preempts_path_out();
                    let init_schedule: PreemptionRecord =
                        PreemptionReader::new(preempts).load_all();
                    eprintln!(
                        ":: {}:\nSchedule:\n {}",
                        "Search successfully found a failing run with schedule:"
                            .green()
                            .bold(),
                        truncated(
                            1000,
                            serde_json::to_string(&init_schedule.clone_preemptions_only()).unwrap()
                        ),
                    );
                }
                return rundat;
            }
            round += 1;
        }
    }

    /// Does the run meet the criteria we are looking for (e.g. a particular error message).
    pub fn output_matches(&self, out: &Output) -> bool {
        let mut answer = true;
        if let Some(pat) = &self.target_stdout {
            let str = String::from_utf8_lossy(&out.stdout);
            if !pat.is_match(&str) {
                if self.verbose {
                    eprintln!("Mismatch for stdout pattern {}", pat);
                    eprintln!("Stdout:\n{}", str);
                }
                answer = false;
            }
        }
        if let Some(pat) = &self.target_stderr {
            let str = String::from_utf8_lossy(&out.stderr);
            if self.verbose {
                eprintln!("Mismatch for stderr pattern {}", pat);
            }
            if !pat.is_match(&str) {
                answer = false;
            }
        }

        if !self.target_exit_code.is_match(out.status) {
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
