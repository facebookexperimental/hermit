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
use std::path::PathBuf;

use anyhow::Context;
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

    runopts: RunOpts,

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

    pub fn _preempts_path_in(&self) -> PathBuf {
        if let Some(p) = &self.runopts.det_opts.det_config.replay_preemptions_from {
            p.to_owned()
        } else {
            self.root_path().with_extension(PREEMPTS_EXT)
        }
    }

    pub fn preempts_path_out(&self) -> PathBuf {
        // TODO: split these apart:
        self.sched_path_out()
    }

    fn sched_path_out(&self) -> PathBuf {
        if let Some(p) = &self.runopts.det_opts.det_config.record_preemptions_to {
            p.to_owned()
        } else {
            self.out_path().with_extension(PREEMPTS_EXT)
        }
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
            _sched_in: None,
            _sched_out: None,
            is_a_match: None,
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
