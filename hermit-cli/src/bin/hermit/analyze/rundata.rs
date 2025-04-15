/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Everything to do with the configuration, inputs, and outputs of a single run: a single point in
//! the search space that `hermit analyze` must navigate.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use colored::Colorize;
use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use detcore::preemptions::strip_times_from_events_file;
use detcore::types::SchedEvent;
use hermit::process::Bind;
use reverie::process::Output;
use tracing::metadata::LevelFilter;

use crate::analyze::consts::*;
use crate::analyze::types::AnalyzeOpts;
use crate::global_opts::GlobalOpts;
use crate::run::RunOpts;

/// A single run plus the results of the run, either in memory or on disk.
pub struct RunData {
    /// A unique name for this run.
    runname: String,
    /// An immutable snapshot of the options.
    analyze_opts: AnalyzeOpts, // Could use an Rc to share 1 copy.

    pub runopts: RunOpts, // TEMP: make private.

    preempts_path_in: Option<PathBuf>,
    sched_path_in: Option<PathBuf>,
    sched_path_out: Option<PathBuf>,

    /// How much logging to enable in the guest.
    log_level: LevelFilter,
    /// Where to write the guest log too (otherwise stderr).
    log_path: Option<PathBuf>,

    /// The input preemptions, if it has been read to memory.
    in_mem_preempts_in: Option<PreemptionRecord>,
    in_mem_sched_out: Option<PreemptionRecord>,

    is_a_match: Option<bool>,
}

impl RunData {
    pub fn root_path(&self) -> PathBuf {
        let tmp_dir = self.analyze_opts.tmp_dir.as_ref().unwrap();
        tmp_dir.join(&self.runname)
    }

    fn out_path(&self) -> PathBuf {
        let tmp_dir = self.analyze_opts.tmp_dir.as_ref().unwrap();
        tmp_dir.join(self.runname.clone() + "_out")
    }

    #[allow(dead_code)]
    pub fn preempts_path_in(&mut self) -> &Path {
        if self.preempts_path_in.is_none() {
            let path = if let Some(p) = &self.runopts.det_opts.det_config.replay_preemptions_from {
                p.to_owned()
            } else {
                self.root_path().with_extension(PREEMPTS_EXT)
            };
            self.preempts_path_in = Some(path);
        }
        self.preempts_path_in.as_ref().unwrap()
    }

    pub fn preempts_path_out(&mut self) -> &Path {
        // TODO: split these apart:
        self.sched_path_out()
    }

    // Return a reference to the in-memory preemption record, reading it from disk if it isn't read
    // already. Errors if the file doesn't exist.
    pub fn preempts_out(&mut self) -> &PreemptionRecord {
        if self.in_mem_sched_out.is_none() {
            let path = self.sched_path_out();
            let pr = PreemptionReader::new(path);
            self.in_mem_sched_out = Some(pr.load_all());
        }
        self.in_mem_sched_out.as_ref().unwrap()
    }

    pub fn sched_path_out(&mut self) -> &Path {
        if self.sched_path_out.is_none() {
            let path = if let Some(p) = &self.runopts.det_opts.det_config.record_preemptions_to {
                p.to_owned()
            } else {
                self.out_path().with_extension(PREEMPTS_EXT)
            };
            self.sched_path_out = Some(path);
        }
        self.sched_path_out.as_ref().unwrap()
    }

    pub fn sched_path_in(&mut self) -> &Path {
        if self.sched_path_in.is_none() {
            let path = if let Some(p) = &self.runopts.det_opts.det_config.replay_schedule_from {
                p.to_owned()
            } else {
                self.root_path().with_extension(PREEMPTS_EXT)
            };
            self.sched_path_in = Some(path);
        }
        self.sched_path_in.as_ref().unwrap()
    }

    /// Convenience function
    pub fn sched_out_file_name(&mut self) -> String {
        self.sched_path_out()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    pub fn sched_out(&mut self) -> &Vec<SchedEvent> {
        let pr = self.preempts_out();
        pr.schedevents()
    }

    /// Only set after launch.
    pub fn log_path(&mut self) -> Option<&PathBuf> {
        if self.has_launched() {
            if self.log_path.is_none() {
                self.log_path = Some(self.root_path().with_extension(LOG_EXT))
            }
            self.log_path.as_ref()
        } else {
            None
        }
    }

    /// Only set after launch.
    pub fn is_a_match(&self) -> bool {
        self.is_a_match.expect("only called after launch method")
    }

    pub fn has_launched(&self) -> bool {
        self.is_a_match.is_some()
    }

    /// Called after the run has been launched, normalize the output preemptions and swap around so
    /// that our output file points to the normalized version.
    pub fn normalize_preempts_out(&mut self) {
        assert!(self.has_launched());
        let preempts_path = self.preempts_path_out();
        let normalized_path = preempts_path.with_extension("normalized");

        let normalized = self.preempts_out().normalize();
        normalized
            .write_to_disk(&normalized_path)
            .expect("write of preempts file to succeed");
        self.in_mem_sched_out = Some(normalized);
        self.sched_path_out = Some(normalized_path);
    }

    /// Execute the run. (Including setting up logging and temp dir binding.)
    pub fn launch(&mut self) -> anyhow::Result<()> {
        let root = self.root_path();
        let log_path = self.root_path().with_extension(LOG_EXT);
        let gopts = GlobalOpts {
            log: Some(self.log_level),
            log_file: if self.analyze_opts.verbose || self.analyze_opts.selfcheck {
                Some(log_path)
            } else {
                None
            },
        };
        let final_record_path = self
            .runopts
            .det_opts
            .det_config
            .record_preemptions_to
            .take();

        // Some last-minute sleight-of-hand to record to a temporary location instead.
        if final_record_path.is_some() {
            let temp_path = root.with_extension(PREEMPTS_EXT_PRESTRIPPED);
            self.runopts.det_opts.det_config.record_preemptions_to = Some(temp_path);
        }

        self.runopts.validate_args();

        let repro_file = self.root_path().with_extension("repro");
        std::fs::write(repro_file, self.to_repro() + "\n")?;

        let (_, output) = self.runopts.run(&gopts, true)?;
        let output: Output = output.context("expected captured output")?;

        File::create(root.with_extension("stdout"))
            .unwrap()
            .write_all(&output.stdout)
            .unwrap();
        File::create(root.with_extension("stderr"))
            .unwrap()
            .write_all(&output.stderr)
            .unwrap();

        let temp_path = self
            .runopts
            .det_opts
            .det_config
            .record_preemptions_to
            .take();
        if let Some(temp_path) = temp_path {
            Self::post_process_events_file(&temp_path, final_record_path.as_ref().unwrap())?;
            // Restore the setting:
            self.runopts.det_opts.det_config.record_preemptions_to = final_record_path;
        }

        self.is_a_match = Some(self.analyze_opts.output_matches(&output));

        if self.analyze_opts.verbose {
            println!(
                "Guest stdout:\n{}",
                String::from_utf8(output.stdout).unwrap()
            );
            println!(
                "Guest stderr:\n{}",
                String::from_utf8(output.stderr).unwrap()
            );
        }
        Ok(())
    }

    /// Any post-processing that needs to be applied to a recorded file of SchedEvents
    pub fn post_process_events_file(sched_path: &Path, dest: &Path) -> anyhow::Result<()> {
        let _ = strip_times_from_events_file(sched_path, Some(dest.to_owned()))?;
        Ok(())
    }

    fn log_level(aopts: &AnalyzeOpts) -> LevelFilter {
        let mut lvl = if let Some(l) = &aopts.guest_log {
            *l
        } else {
            LevelFilter::WARN
        };

        // Ensure a minum level for these functionalities:
        if aopts.verbose || aopts.selfcheck && lvl < LevelFilter::DEBUG {
            if aopts.guest_log.is_some() {
                eprintln!(
                    "WARNING: manually set log lvl to {} but need DEBUG for selfcheck/verbose functionality",
                    lvl
                );
            }
            lvl = LevelFilter::DEBUG;
        }

        lvl
    }

    pub fn new(aopts: &AnalyzeOpts, runname: String, runopts: RunOpts) -> Self {
        let log_level: LevelFilter = Self::log_level(aopts);
        let mut rd = RunData {
            runname,
            analyze_opts: aopts.clone(),
            runopts,
            preempts_path_in: None,
            sched_path_in: None,
            sched_path_out: None,
            log_path: None,
            in_mem_preempts_in: None,
            in_mem_sched_out: None,
            is_a_match: None,
            log_level,
        };

        // By default we save the config for every run.
        let conf_file = rd.root_path().with_extension("config");
        rd.runopts.save_config = Some(conf_file);

        let summary_file = rd.root_path().with_extension("summary");
        rd.runopts.summary_json = Some(summary_file);

        rd
    }

    /// Create a new run with the baseline RunOpts created from the AnalyzeOpts
    pub fn new_baseline(aopts: &AnalyzeOpts, runname: String) -> anyhow::Result<Self> {
        let ro = Self::get_base_runopts(aopts)?;
        Ok(Self::new(aopts, runname, ro))
    }

    /// Create a run for the initial on-target execution.
    pub fn new_run1_target(aopts: &AnalyzeOpts, runname: String) -> anyhow::Result<Self> {
        let ro = Self::get_run1_runopts(aopts)?;
        Ok(Self::new(aopts, runname, ro))
    }

    /// The baseline RunOpts based on user flags plus some sanitation/validation.
    fn get_base_runopts(aopts: &AnalyzeOpts) -> anyhow::Result<RunOpts> {
        let mut ro = Self::get_raw_runopts(aopts);
        if ro.no_sequentialize_threads {
            bail!(
                "Error, cannot search through executions with --no-sequentialize-threads.  Determinism required.",
            )
        }

        // We could add a flag for analyze-without chaos, but it's a rare use case that isn't
        // usefully supported now anyway.  Exploring with RNG alone doesn't make sense, but we may
        // want to make it possible to do analyze with the stick random scheduler instead of the
        // one.
        ro.det_opts.det_config.chaos = true;

        ro.validate_args();
        assert!(ro.det_opts.det_config.sequentialize_threads);
        if aopts.run1_seed.is_some() && !ro.det_opts.det_config.chaos {
            eprintln!(
                "{}",
                "WARNING: --chaos not in supplied hermit run args, but --run1-seed is.  Usually this is an error."
                    .bold()
                    .red()
            )
        }
        Self::runopts_add_binds(aopts, &mut ro)?;

        Ok(ro)
    }

    fn runopts_add_binds(aopts: &AnalyzeOpts, runopts: &mut RunOpts) -> anyhow::Result<()> {
        let bind_dir: Bind = Bind::from_str(aopts.get_tmp()?.to_str().unwrap())?;
        runopts.bind.push(bind_dir);
        runopts.validate_args();
        Ok(())
    }

    /// The raw, unvarnished, RunOpts.
    fn get_raw_runopts(aopts: &AnalyzeOpts) -> RunOpts {
        // Bogus arg 0 for CLI argument parsing:
        let mut run_cmd: Vec<String> = vec!["hermit-run".to_string()];

        for arg in &aopts.run_arg {
            run_cmd.push(arg.to_string());
        }
        for arg in &aopts.run_args {
            run_cmd.push(arg.to_string());
        }
        RunOpts::from_iter(run_cmd.iter())
    }

    /// Extract the (initial) RunOpts for target/run1 that are implied by all of hermit analyze's arguments.
    fn get_run1_runopts(aopts: &AnalyzeOpts) -> anyhow::Result<RunOpts> {
        let mut ro = Self::get_base_runopts(aopts)?;

        // If there was a --sched-seed specified in run_args, it is overridden by this setting:
        if let Some(seed) = aopts.run1_seed {
            ro.det_opts.det_config.seed = seed;
        } else if let Some(path) = &aopts.run1_preemptions {
            ro.det_opts.det_config.replay_preemptions_from = Some(path.clone());
        }
        Ok(ro)
    }

    /// A temporary constructor method until minimize overhaul is complete and it returns a RunData directly.
    pub fn from_minimize_output(
        aopts: &AnalyzeOpts,
        runname: String,
        runopts: RunOpts,
        in_mem_preempts: PreemptionRecord,
        preempts_path: PathBuf,
        log_path: PathBuf,
    ) -> Self {
        let log_level: LevelFilter = Self::log_level(aopts);
        RunData {
            runname,
            analyze_opts: aopts.clone(),
            runopts,
            preempts_path_in: None,
            sched_path_in: None,
            sched_path_out: Some(preempts_path),
            log_path: Some(log_path),
            log_level,
            in_mem_sched_out: Some(in_mem_preempts),
            in_mem_preempts_in: None,
            // Invariant: minimize should always return an on-target configuration:
            is_a_match: Some(true),
        }
    }

    /// Another fake run that stores a result without actually launching anything.
    pub fn from_schedule_trace(
        aopts: &AnalyzeOpts,
        runname: String,
        runopts: RunOpts,
        sched_path: PathBuf,
    ) -> Self {
        let log_level: LevelFilter = Self::log_level(aopts);
        RunData {
            runname,
            analyze_opts: aopts.clone(),
            runopts,
            preempts_path_in: None,
            sched_path_in: None,
            sched_path_out: Some(sched_path),
            log_path: None,
            log_level,
            in_mem_sched_out: None,
            in_mem_preempts_in: None,
            // Don't claim that it was run:
            is_a_match: None,
        }
    }

    pub fn with_preempts_path_in(mut self, path: PathBuf) -> Self {
        self.runopts.det_opts.det_config.replay_preemptions_from = Some(path);
        self
    }

    pub fn with_preempts_in(mut self, pr: PreemptionRecord) -> Self {
        let path = self.preempts_path_in().to_path_buf();
        pr.write_to_disk(&path)
            .expect("write of preempts file to succeed");
        self.in_mem_preempts_in = Some(pr);
        self.with_preempts_path_in(path)
    }

    pub fn with_preemption_recording(self) -> Self {
        let path = self.out_path().with_extension(PREEMPTS_EXT);
        self.with_preemption_recording_to(path)
    }

    pub fn with_preemption_recording_to(mut self, path: PathBuf) -> Self {
        self.runopts.det_opts.det_config.record_preemptions_to = Some(path);
        self
    }

    // TODO: separate from preemption recording
    pub fn with_schedule_recording(self) -> Self {
        self.with_preemption_recording()
    }

    // TODO: separate from preemption recording
    pub fn with_schedule_recording_to(self, path: PathBuf) -> Self {
        self.with_preemption_recording_to(path)
    }

    /// Replay from the default location, as returned by sched_path_in
    pub fn with_schedule_replay(mut self) -> Self {
        let path = self.sched_path_in().to_owned();
        self.with_schedule_replay_from(path)
    }

    pub fn with_schedule_replay_from(mut self, path: PathBuf) -> Self {
        self.runopts.det_opts.det_config.replay_schedule_from = Some(path);
        self
    }

    pub fn to_repro(&self) -> String {
        let logging = if let Some(path) = &self.log_path {
            format!(" --log=debug --log-file={}", path.display())
        } else {
            "".to_string()
        };
        // let logging = if self.analyze_opts.verbose || self.analyze_opts.selfcheck {
        //     let path = self.log_path().unwrap();
        //     format!(" --log=debug --log-file={}", path.display())
        // } else {
        //     "".to_string()
        // };
        format!("hermit{} run {}", logging, self.runopts)
    }

    pub fn into_runopts(self) -> RunOpts {
        self.runopts
    }
}
