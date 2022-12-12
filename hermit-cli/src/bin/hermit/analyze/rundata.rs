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

use anyhow::Context;
use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use detcore::types::SchedEvent;
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
    sched_path_out: Option<PathBuf>,
    in_mem_sched_out: Option<PreemptionRecord>,

    /// The input schedule, if it has been read to memory.
    _sched_in: Option<Vec<SchedEvent>>,
    _sched_out: Option<Vec<SchedEvent>>,

    is_a_match: Option<bool>,
}

impl RunData {
    fn root_path(&self) -> PathBuf {
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
    // already.
    pub fn preempts_out(&mut self) -> &PreemptionRecord {
        if self.in_mem_sched_out.is_none() {
            let path = self.sched_path_out();
            let pr = PreemptionReader::new(path);
            self.in_mem_sched_out = Some(pr.load_all());
            // if let Some(path) = &self.sched_path_out {
            //     let pr = PreemptionReader::new(preempts_path);
            //     self.in_mem_sched_out = Some(pr.load_all());
            // } else {
            //     panic!("preempts_out should only be called after sched_path_out is established");
            // }
        }
        self.in_mem_sched_out.as_ref().unwrap()
    }

    pub fn sched_path_out(&mut self) -> &Path {
        if self.sched_path_out.is_none() {
            let path = if let Some(p) = &self.runopts.det_opts.det_config.record_preemptions_to {
                p.to_owned()
            } else {
                self.root_path().with_extension(PREEMPTS_EXT)
            };
            self.sched_path_out = Some(path);
        }
        self.sched_path_out.as_ref().unwrap()
    }

    /// Only set after launch.
    pub fn log_path(&self) -> Option<PathBuf> {
        if self.is_a_match.is_some() {
            Some(self.root_path().with_extension(LOG_EXT))
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
        let root = self.analyze_opts.get_tmp()?.join(&self.runname);
        let log_path = self.root_path().with_extension(LOG_EXT);
        self.analyze_opts
            .print_and_validate_runopts(&mut self.runopts, &self.runname);

        let conf_file = root.with_extension("config");
        self.runopts.save_config = Some(conf_file);

        let gopts = if self.analyze_opts.verbose || self.analyze_opts.selfcheck {
            GlobalOpts {
                log: Some(LevelFilter::DEBUG),
                log_file: Some(log_path),
            }
        } else {
            NO_LOGGING.clone()
        };

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

    pub fn new(aopts: &AnalyzeOpts, runname: String, runopts: RunOpts) -> Self {
        RunData {
            runname,
            analyze_opts: aopts.clone(),
            runopts,
            preempts_path_in: None,
            sched_path_out: None,
            in_mem_sched_out: None,
            _sched_in: None,
            _sched_out: None,
            is_a_match: None,
        }
    }

    /// A temporary constructor method until minimize overhaul is complete and it returns a RunData directly.
    pub fn from_minimize_output(
        aopts: &AnalyzeOpts,
        runname: String,
        runopts: RunOpts,
        in_mem_preempts: PreemptionRecord,
        preempts_path: PathBuf,
        _log_path: PathBuf,
    ) -> Self {
        RunData {
            runname,
            analyze_opts: aopts.clone(),
            runopts,
            preempts_path_in: None,
            sched_path_out: Some(preempts_path),
            in_mem_sched_out: Some(in_mem_preempts),
            _sched_in: None,
            _sched_out: None,
            // Invariant: minimize should always return an on-target configuration:
            is_a_match: Some(true),
        }
    }

    fn _replay_preemptions_from(&mut self, path: PathBuf) {
        self.runopts.det_opts.det_config.replay_preemptions_from = Some(path);
    }

    pub fn with_preemption_recording(mut self) -> Self {
        let path = self.out_path().with_extension(PREEMPTS_EXT);
        self.runopts.det_opts.det_config.record_preemptions_to = Some(path);
        self
    }

    pub fn to_repro(&self) -> String {
        self.analyze_opts
            .runopts_to_repro(&self.runopts, Some(&self.runname))
    }
}
