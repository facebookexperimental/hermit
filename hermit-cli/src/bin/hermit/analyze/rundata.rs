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
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use clap::parser::ValueSource;
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

    /// The global options a trial run is launched with. Trials go through
    /// `RunOpts::run`, which dispatches on the trial `RunOpts`' own backend (set
    /// from `hermit --backend <BACKEND> analyze` by `get_raw_runopts`); this
    /// copy keeps the trial's global options consistent with it.
    fn trial_global_opts(&self) -> GlobalOpts {
        let log_path = self.root_path().with_extension(LOG_EXT);
        GlobalOpts {
            log: Some(self.log_level),
            log_file: if self.analyze_opts.verbose || self.analyze_opts.selfcheck {
                Some(log_path)
            } else {
                None
            },
            backend: self.analyze_opts.backend,
            // Every trial charges the invocation's ONE budget, as verify's
            // per-run logs do. Without --verbose/--selfcheck a trial traces to
            // hermit's own stderr -- the public stream the cap exists to bound.
            max_log_bytes: self.analyze_opts.max_log_bytes,
            log_budget: self.analyze_opts.log_budget.clone(),
            log_file_handle: None,
            unsafe_ignore_host_seccomp: false,
        }
    }

    /// Execute the run. (Including setting up logging and temp dir binding.)
    pub fn launch(&mut self) -> anyhow::Result<()> {
        let root = self.root_path();
        let gopts = self.trial_global_opts();
        // Open it HERE, on the host, for the same reason `main` does: `launch` runs
        // before the container exists, and opening later would resolve this path in
        // the guest namespace. Analyze's log lands under the run's own directory, so
        // it is only exposed when that directory is itself inside the container's
        // /tmp -- but the fix is the same and costs nothing.
        let mut gopts = gopts;
        gopts.open_log_file()?;
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

        self.runopts.validate_args()?;

        let repro_file = self.root_path().with_extension("repro");
        std::fs::write(repro_file, self.to_repro() + "\n")?;
        self.runopts.write_debug_options_dump()?;

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

    /// Replay with the selected run's seeds and the original replay policy.
    pub fn new_run1_replay(
        aopts: &AnalyzeOpts,
        runname: String,
        selected: &RunOpts,
    ) -> anyhow::Result<Self> {
        let mut replay = Self::new_run1_target(aopts, runname)?;
        let target = &selected.det_opts.det_config;
        let config = &mut replay.runopts.det_opts.det_config;
        // Search chooses an effective scheduler seed independently of the
        // initial command line. Preserve all selected seed inputs, without
        // copying search-only overrides such as --imprecise-search into replay.
        config.seed = target.seed;
        config.rng_seed = target.rng_seed;
        config.fuzz_seed = target.fuzz_seed;
        config.sched_seed = target.sched_seed;
        Ok(replay)
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

        ro.validate_args()?;
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
        runopts.validate_args()?;
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
        let mut runopts = RunOpts::try_parse_from(run_cmd.iter()).unwrap_or_else(|error| {
            if crate::misplaced_backend_argument(&error).is_some() {
                // `--backend` is global. Point at the spelling that selects the
                // trials' backend instead of letting clap suggest the unrelated
                // `--backend-engagement-json`. clap reports only the flag name,
                // so read the value from the run arguments themselves.
                // The first `--backend` is the one clap refused; later tokens may
                // be the guest's own arguments.
                let value = run_cmd
                    .iter()
                    .enumerate()
                    .find_map(|(index, arg)| {
                        if arg == "--backend" {
                            Some(run_cmd.get(index + 1).map(String::as_str).unwrap_or(""))
                        } else {
                            arg.strip_prefix("--backend=")
                        }
                    })
                    .filter(|value| !value.is_empty() && !value.starts_with('-'))
                    .unwrap_or("<BACKEND>");
                clap::Error::raw(
                    clap::error::ErrorKind::UnknownArgument,
                    format!(
                        "`--backend` is not a `run` option, so it cannot be passed through \
                         the run arguments of `hermit analyze` or `hermit bisect`. It is a \
                         global option: select the backend for every trial with `hermit \
                         --backend={value} analyze ...` or `hermit --backend={value} bisect \
                         ...`.\n"
                    ),
                )
                .exit()
            }
            error.exit()
        });
        // Apply the global backend before `get_base_runopts` validates these
        // options, so backend-specific validation sees the backend the trials
        // will run on.
        runopts.set_backend(aopts.backend);
        runopts.set_inexact_branch_counter(aopts.inexact_branch_counter);
        runopts
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
        format!(
            "hermit{}{} run {}",
            logging,
            self.runopts.global_backend_arg(),
            self.runopts
        )
    }

    pub fn into_runopts(self) -> RunOpts {
        self.runopts
    }
}

impl AnalyzeOpts {
    /// Refuses `--max-log-bytes` (exit 122) where the cap's `_exit(123)` could
    /// leave a trial's guest running (`RunOpts::log_cap_refusal`). `run` makes
    /// this check in `RunOpts::main`, but trials go straight to `RunOpts::run`,
    /// so `analyze` and `bisect` make it here, before any workspace or trial
    /// exists.
    pub fn refuse_unsupervised_log_cap(&self) -> anyhow::Result<()> {
        let trial = RunData::get_raw_runopts(self);
        trial.refuse_unsupervised_log_cap(self.max_log_bytes)
    }

    /// Refuses `--strict` in the run arguments (exit 122) where the trials'
    /// backend reads its virtual clock from a retired-branch counter that
    /// fails Reverie's validation, as `hermit run --strict` does in
    /// `RunOpts::main`. Trials go straight to `RunOpts::run`, so `analyze` and
    /// `bisect` make the check here, before any workspace or trial exists.
    /// Without it a trial on a miscounting counter can disagree with another
    /// for a hardware reason and be reported as a race
    /// (https://github.com/rrnewton/hermit/issues/3810). Unlike `run`, a trial
    /// with `--namespace-only` is not exempt: trials never reach the
    /// namespace-only launcher, so they still run on ptrace.
    pub fn refuse_strict_with_inexact_branch_counter(&self) -> anyhow::Result<()> {
        RunData::get_raw_runopts(self).refuse_strict_trial_with_inexact_branch_counter()
    }

    /// Installs the trials' `--skid-margin`, as `RunOpts::main` does for a
    /// run: Reverie's PMU configuration is per process, and every trial of
    /// one `analyze` or `bisect` runs in this process with the same run
    /// arguments. It must precede the strict counter refusal, whose
    /// validation reads that configuration. Before this, trials accepted
    /// `--skid-margin`, ran with the default margin, and printed a
    /// reproducer that applied it.
    pub fn install_trial_pmu_config(&self) -> anyhow::Result<()> {
        RunData::get_raw_runopts(self).install_pmu_config()
    }

    /// Replay the given records from the virtual-time epoch they were
    /// recorded under, as `hermit run` does when it replays one
    /// (`RunOpts::adopt_replayed_schedule_epoch`): every trial gets that epoch
    /// as `--epoch`. A record's times are absolute virtual times measured from
    /// its own epoch, and trials start through `RunOpts::run`, which does not
    /// reconcile epochs, so without this they would start from the run
    /// arguments' epoch instead.
    ///
    /// A network trace that the run arguments replay (`--replay-networking`)
    /// is reconciled with them: its inputs are released at absolute virtual
    /// times too (https://github.com/rrnewton/hermit/issues/3875).
    ///
    /// Records stored under two different epochs, or an explicit epoch
    /// (`--epoch` or `HERMIT_EPOCH`) that differs from theirs, are refused
    /// before any trial. Records written before epochs were stored carry none
    /// and change nothing. Each recording is named by its option, such as
    /// `--good`, and its path.
    pub fn adopt_recorded_epoch(&mut self, recordings: &[(&str, &Path)]) -> anyhow::Result<()> {
        let run_cmd = std::iter::once("hermit-run").chain(
            self.run_arg
                .iter()
                .chain(&self.run_args)
                .map(String::as_str),
        );
        let matches = RunOpts::command()
            .try_get_matches_from(run_cmd)
            .context("cannot parse the trials' run arguments")?;
        let parsed = RunOpts::from_arg_matches(&matches)?;
        let mut epochs = Vec::new();
        for (option, path) in recordings {
            let epoch =
                detcore::preemptions::read_recorded_epoch(path).map_err(anyhow::Error::msg)?;
            epochs.push((format!("{option} {}", path.display()), epoch));
        }
        // A network trace given in the run arguments releases its inputs at
        // absolute virtual times too, and every trial replays it.
        if let Some(path) = parsed.replay_networking_trace() {
            let file = File::open(path)
                .with_context(|| format!("cannot open network trace {}", path.display()))?;
            let trace = detcore_model::network_trace::NetworkTraceV2::read_framed(
                std::io::BufReader::new(file),
            )
            .with_context(|| format!("{} is not a valid network trace", path.display()))?;
            epochs.push((
                format!("--replay-networking {}", path.display()),
                Some(trace.epoch),
            ));
        }
        let mut recorded = None;
        let mut sources: Vec<String> = Vec::new();
        for (source, epoch) in epochs {
            let Some(epoch) = epoch else {
                continue;
            };
            match recorded {
                None => recorded = Some(epoch),
                Some(first) if first != epoch => {
                    return Err(anyhow::Error::new(crate::container::PolicyRefusal).context(
                        format!(
                            "{} was recorded under the virtual-time epoch {} and {source} \
                             under {}. Each record's times are measured from its own epoch, \
                             so no single replay epoch reproduces both. Record both with the \
                             same --epoch.",
                            sources[0],
                            first.to_rfc3339(),
                            epoch.to_rfc3339(),
                        ),
                    ));
                }
                Some(_) => {}
            }
            sources.push(source);
        }
        let Some(recorded) = recorded else {
            return Ok(());
        };
        let recorded_text = recorded.to_rfc3339();
        if matches.value_source("epoch") == Some(ValueSource::DefaultValue) {
            self.run_arg.push(format!("--epoch={recorded_text}"));
            return Ok(());
        }
        let explicit = parsed.det_opts.det_config.epoch;
        if explicit == recorded {
            return Ok(());
        }
        Err(
            anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                "the explicit virtual-time epoch {} (from --epoch or HERMIT_EPOCH) differs from \
                 the epoch {recorded_text} that {} {} recorded under. A record's times are \
                 absolute virtual times measured from its own epoch, so replaying it from \
                 another epoch cannot reproduce the run. Omit --epoch to replay from the \
                 recorded epoch, or pass --epoch={recorded_text}.",
                explicit.to_rfc3339(),
                sources.join(" and "),
                if sources.len() == 1 { "was" } else { "were" },
            )),
        )
    }

    /// The virtual-time epoch the target trial (run 1) starts from, in
    /// RFC 3339. It replays `--run1-preemptions` when that is given.
    #[cfg(test)]
    pub(crate) fn trial_epoch_for_test(&self) -> String {
        RunData::new_run1_target(self, "epoch-probe".to_owned())
            .unwrap()
            .runopts
            .det_opts
            .det_config
            .epoch
            .to_rfc3339()
    }

    /// Refuses (exit 122) `--save-config` and `--summary-json` in the run
    /// arguments, before any workspace, schedule read or trial exists. Every
    /// trial writes its configuration and summary into the workspace under
    /// its own name (`RunData::new` replaces both paths), so the requested
    /// files would never be written. `--record-networking` is refused too:
    /// it would put every trial on the host network and have every trial
    /// write the same trace. So are the options `RunOpts::options_trials_refuse`
    /// lists: host-network and namespace-less routes, and output paths every
    /// trial would share.
    pub fn refuse_outputs_trials_overwrite(&self) -> anyhow::Result<()> {
        let trial = RunData::get_raw_runopts(self);
        // A recording runs on the host network (`RunOpts::validate_args`), so
        // the trials would stop being reproducible, and every trial would
        // write the same trace path.
        if let Some(path) = trial.record_networking_trace() {
            return Err(
                anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                    "`hermit analyze` and `hermit bisect` refuse --record-networking={} in the \
                     run arguments: it puts every trial on the host network, so the trials \
                     are no longer reproducible, and every trial writes that same trace, so \
                     only the last one would remain. Record the network once with `hermit run \
                     --record-networking`, then pass the trace to the trials with \
                     --replay-networking.",
                    path.display()
                )),
            );
        }
        let refused = trial.options_trials_refuse();
        if !refused.is_empty() {
            return Err(
                anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                    "`hermit analyze` and `hermit bisect` refuse {} in the run arguments: {}. \
                     Remove {} from the run arguments.",
                    refused
                        .iter()
                        .map(|(option, _)| *option)
                        .collect::<Vec<_>>()
                        .join(" and "),
                    refused
                        .iter()
                        .map(|(option, reason)| format!("with {option}, {reason}"))
                        .collect::<Vec<_>>()
                        .join("; "),
                    if refused.len() == 1 { "it" } else { "them" },
                )),
            );
        }
        let overwritten: Vec<&str> = [
            (trial.save_config.is_some(), "--save-config"),
            (trial.summary_json.is_some(), "--summary-json"),
        ]
        .into_iter()
        .filter_map(|(set, option)| set.then_some(option))
        .collect();
        if overwritten.is_empty() {
            return Ok(());
        }
        Err(
            anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                "`hermit analyze` and `hermit bisect` write each trial's configuration and summary \
             into their workspace, so {} would never be written. Remove {} from the run \
             arguments; the workspace keeps both files for every trial.",
                overwritten.join(" and "),
                if overwritten.len() == 1 { "it" } else { "them" },
            )),
        )
    }

    /// Refuses `--timeout` (exit 122) on a trial backend where `hermit run`
    /// refuses it (`RunOpts::ensure_timeout_supported`), before any workspace,
    /// schedule read or trial exists. Trials never pass through
    /// `RunOpts::main`, which makes that check for a run, so a KVM trial
    /// accepted the bound that its printed `hermit --backend=kvm run
    /// --timeout=N` reproducer is refused for.
    pub fn refuse_unqualified_trial_timeout(&self) -> anyhow::Result<()> {
        RunData::get_raw_runopts(self).ensure_timeout_supported()
    }

    /// Refuses (exit 122) run arguments that a trial would accept and run
    /// without (`RunOpts::options_only_main_applies`), before any workspace,
    /// schedule read or trial exists. Accepting them would run every trial
    /// without them and print a reproducer that applies them, so the
    /// reproducer would not reproduce the trial.
    pub fn refuse_options_trials_do_not_apply(&self) -> anyhow::Result<()> {
        let dropped = RunData::get_raw_runopts(self).options_only_main_applies();
        if dropped.is_empty() {
            return Ok(());
        }
        Err(
            anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                "`hermit analyze` and `hermit bisect` cannot apply {} to their trials: each trial \
             starts through `RunOpts::run`, and only `hermit run` itself applies {}. Remove {} \
             from the run arguments, or run the program with `hermit run`.",
                dropped.join(", "),
                if dropped.len() == 1 { "it" } else { "them" },
                if dropped.len() == 1 { "it" } else { "them" },
            )),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-2 review of https://github.com/rrnewton/hermit/pull/3686, finding
    /// 2: the refusal `RunOpts::main` makes never sees an analyze or bisect
    /// trial, which goes straight to `RunOpts::run`. Those commands admit
    /// ptrace and KVM, and `--no-namespace`, passed through the run arguments,
    /// is refused with either (round-4c review, finding 1).
    #[test]
    fn log_cap_refusal_reaches_analyze_and_bisect_trials() {
        let options = |argv: &[&str]| {
            let args = crate::Args::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options
        };
        let kvm = "--backend=kvm and --no-namespace";
        for (refused, named) in [
            (
                &[
                    "hermit",
                    "--max-log-bytes=4K",
                    "--backend=kvm",
                    "analyze",
                    "--run-arg=--no-namespace",
                    "--",
                    "/bin/true",
                ][..],
                kvm,
            ),
            (
                &[
                    "hermit",
                    "--max-log-bytes=4K",
                    "--backend=kvm",
                    "analyze",
                    "--",
                    "--no-namespace",
                    "/bin/true",
                ][..],
                kvm,
            ),
            // The default (ptrace) backend: its tracer sets PTRACE_O_EXITKILL
            // only after the trial's guest exists.
            (
                &[
                    "hermit",
                    "--max-log-bytes=4K",
                    "analyze",
                    "--run-arg=--no-namespace",
                    "--",
                    "/bin/true",
                ][..],
                "--no-namespace",
            ),
            (
                &[
                    "hermit",
                    "--max-log-bytes=4K",
                    "analyze",
                    "--",
                    "--no-namespace",
                    "/bin/true",
                ][..],
                "--no-namespace",
            ),
        ] {
            let error = options(refused)
                .refuse_unsupervised_log_cap()
                .expect_err("the trial's guest could outlive hermit");
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{refused:?}: {error:#}"
            );
            assert!(
                error.to_string().starts_with(&format!(
                    "--max-log-bytes cannot be enforced with {named}: "
                )),
                "{refused:?}: {error:#}"
            );
        }
        for accepted in [
            &[
                "hermit",
                "--backend=kvm",
                "analyze",
                "--run-arg=--no-namespace",
                "--",
                "/bin/true",
            ][..],
            &[
                "hermit",
                "--max-log-bytes=4K",
                "--backend=kvm",
                "analyze",
                "--",
                "/bin/true",
            ][..],
            &[
                "hermit",
                "analyze",
                "--run-arg=--no-namespace",
                "--",
                "/bin/true",
            ][..],
            &["hermit", "--max-log-bytes=4K", "analyze", "--", "/bin/true"][..],
        ] {
            options(accepted)
                .refuse_unsupervised_log_cap()
                .unwrap_or_else(|error| panic!("{accepted:?}: {error:#}"));
        }
    }

    /// https://github.com/rrnewton/hermit/issues/3810: `hermit run --strict`
    /// refuses an inexact retired-branch counter in `RunOpts::main`, which
    /// analyze and bisect trials never reach, so the trial options make the
    /// same refusal, with the same conditions: `--strict` in either spelling of
    /// the run arguments, a backend that reads that counter, and a failed
    /// validation.
    #[test]
    fn strict_trials_refuse_an_inexact_branch_counter() {
        let options = |argv: &[&str], probe: fn() -> Option<String>| {
            let args = crate::Args::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options.inexact_branch_counter = probe;
            options
        };
        let inexact = || Some("SpecLockMap is enabled".to_string());
        for refused in [
            &["hermit", "analyze", "--run-arg=--strict", "--", "/bin/true"][..],
            &["hermit", "analyze", "--", "--strict", "/bin/true"][..],
            &[
                "hermit",
                "--backend=ptrace",
                "analyze",
                "--",
                "--strict",
                "/bin/true",
            ][..],
            // `RunOpts::run` ignores `--namespace-only`, so these trials still
            // run on ptrace and its counter (rel-041 review of
            // https://github.com/rrnewton/hermit/pull/3834).
            &[
                "hermit",
                "analyze",
                "--",
                "--strict",
                "--namespace-only",
                "/bin/true",
            ][..],
            &[
                "hermit",
                "analyze",
                "--run-arg=--strict",
                "--run-arg=--lite",
                "--",
                "/bin/true",
            ][..],
        ] {
            let error = options(refused, inexact)
                .refuse_strict_with_inexact_branch_counter()
                .expect_err("a strict trial on an inexact counter");
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{refused:?}: {error:#}"
            );
            let message = error.to_string();
            for expected in ["--strict", "`ptrace` backend", "SpecLockMap is enabled"] {
                assert!(
                    message.contains(expected),
                    "{refused:?}: {expected:?}: {message}"
                );
            }
        }
        let unreachable: fn() -> Option<String> = || panic!("the counter is not consulted");
        for (accepted, probe) in [
            (&["hermit", "analyze", "--", "/bin/true"][..], unreachable),
            (
                &[
                    "hermit",
                    "--backend=kvm",
                    "analyze",
                    "--",
                    "--strict",
                    "/bin/true",
                ][..],
                unreachable,
            ),
            (
                &["hermit", "analyze", "--", "--strict", "/bin/true"][..],
                || None,
            ),
        ] {
            options(accepted, probe)
                .refuse_strict_with_inexact_branch_counter()
                .unwrap_or_else(|error| panic!("{accepted:?}: {error:#}"));
        }
    }

    /// Trials start through `RunOpts::run`, so run options that only
    /// `RunOpts::main` applies are refused, each named, instead of being
    /// dropped from every trial while the printed reproducer keeps them.
    /// Options the trials do apply are admitted.
    #[test]
    fn trials_refuse_run_options_only_main_applies() {
        let options = |argv: &[&str]| {
            let args = crate::Args::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options
        };
        for (run_args, named) in [
            (&["--namespace-only"][..], &["--namespace-only"][..]),
            (&["--lite"][..], &["--namespace-only"][..]),
            (&["--verify"][..], &["--verify"][..]),
            (&["--verify", "--verify-strict"][..], &["--verify"][..]),
            (&["--verify-run=vr"][..], &["--verify-run"][..]),
            (
                &["--backend-engagement-json=b.json"][..],
                &["--backend-engagement-json"][..],
            ),
            (
                &["--verify", "--happens-before=hb.txt"][..],
                &["--verify", "--happens-before"][..],
            ),
        ] {
            let mut argv = vec!["hermit", "analyze", "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            let error = options(&argv)
                .refuse_options_trials_do_not_apply()
                .expect_err("a dropped run option");
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{argv:?}: {error:#}"
            );
            let message = error.to_string();
            for option in named {
                assert!(message.contains(option), "{argv:?}: {option}: {message}");
            }
        }
        for run_args in [
            &[][..],
            &["--strict"][..],
            &["--no-namespace"][..],
            &["--timeout=5"][..],
            &["--skid-margin=500"][..],
            &["--summary"][..],
        ] {
            let mut argv = vec!["hermit", "analyze", "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            options(&argv)
                .refuse_options_trials_do_not_apply()
                .unwrap_or_else(|error| panic!("{argv:?}: {error:#}"));
        }
    }

    /// rel-041's review of https://github.com/rrnewton/hermit/pull/3836: an
    /// explicit `--save-config` or `--summary-json` is refused, each named,
    /// because every trial writes both into the workspace under its own
    /// name. Without them a trial still gets those workspace paths.
    #[test]
    fn trials_refuse_outputs_they_overwrite() {
        let options = |run_args: &[&str]| {
            let mut argv = vec!["analyze", "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            AnalyzeOpts::try_parse_from(&argv).unwrap()
        };
        for (run_args, named) in [
            (
                &["--save-config=requested.config"][..],
                &["--save-config"][..],
            ),
            (
                &["--summary-json=requested.summary"][..],
                &["--summary-json"][..],
            ),
            (
                &["--save-config=c", "--summary-json=s"][..],
                &["--save-config", "--summary-json"][..],
            ),
        ] {
            let error = options(run_args)
                .refuse_outputs_trials_overwrite()
                .expect_err("an overwritten output");
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{run_args:?}: {error:#}"
            );
            let message = error.to_string();
            for option in named {
                assert!(
                    message.contains(option),
                    "{run_args:?}: {option}: {message}"
                );
            }
        }
        let mut admitted = options(&[]);
        admitted.refuse_outputs_trials_overwrite().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        admitted.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&admitted, "outputs".to_owned()).unwrap();
        for path in [&run.runopts.save_config, &run.runopts.summary_json] {
            let path = path.as_ref().expect("the trial writes it");
            assert!(path.starts_with(workspace.path()), "{}", path.display());
        }
        // A network recording would put every trial on the host network.
        let error = options(&["--record-networking=requested.trace"])
            .refuse_outputs_trials_overwrite()
            .expect_err("a network recording");
        assert!(
            error
                .downcast_ref::<crate::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        let message = error.to_string();
        assert!(
            message.contains("refuse --record-networking=requested.trace")
                && message.contains("host network")
                && message.contains("--replay-networking"),
            "{message}"
        );
        // Host-network and namespace-less routes, and output paths every
        // trial would share (`RunOpts::options_trials_refuse`).
        for (run_arg, named) in [
            ("--strace-only", "--strace-only"),
            ("--no-sequentialize-threads", "--no-sequentialize-threads"),
            ("--network=host", "--network=host"),
            ("--no-namespace", "--no-namespace"),
            ("--gdbserver", "--gdbserver"),
            (
                "--record-preemptions-to=requested.preempts",
                "--record-preemptions-to",
            ),
            (
                "--stacktrace-event=5,requested.stack",
                "--stacktrace-event with a path",
            ),
            (
                "--preemption-stacktrace-log-file=requested.log",
                "--preemption-stacktrace-log-file",
            ),
        ] {
            let error = options(&[run_arg])
                .refuse_outputs_trials_overwrite()
                .expect_err("an option the trials cannot take");
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{run_arg}: {error:#}"
            );
            let message = error.to_string();
            assert!(
                message.contains(&format!("refuse {named} in the run arguments"))
                    && message.contains(&format!("with {named}, ")),
                "{run_arg}: {message}"
            );
        }
        // A stack-trace event without a path prints to stderr, and local
        // networking is the default: both stay admitted.
        for admitted in ["--stacktrace-event=5", "--network=local"] {
            options(&[admitted])
                .refuse_outputs_trials_overwrite()
                .unwrap_or_else(|error| panic!("{admitted}: {error:#}"));
        }
    }

    /// rel-041's review of https://github.com/rrnewton/hermit/pull/3836:
    /// `--timeout` on a trial backend where `hermit run` refuses it (KVM) is
    /// refused the same way, while the ptrace trial keeps it.
    #[test]
    fn trials_refuse_a_timeout_their_backend_cannot_enforce() {
        let options = |argv: &[&str]| {
            let args = crate::Args::try_parse_from(argv).unwrap();
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options
        };
        let error = options(&[
            "hermit",
            "--backend=kvm",
            "analyze",
            "--",
            "--timeout=3",
            "/bin/true",
        ])
        .refuse_unqualified_trial_timeout()
        .expect_err("KVM does not enforce --timeout");
        assert!(
            error
                .downcast_ref::<crate::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("--timeout is not qualified"),
            "{error:#}"
        );
        for admitted in [
            &["hermit", "analyze", "--", "--timeout=3", "/bin/true"][..],
            &[
                "hermit",
                "--backend=ptrace",
                "analyze",
                "--",
                "--timeout=3",
                "/bin/true",
            ][..],
            &["hermit", "--backend=kvm", "analyze", "--", "/bin/true"][..],
        ] {
            options(admitted)
                .refuse_unqualified_trial_timeout()
                .unwrap_or_else(|error| panic!("{admitted:?}: {error:#}"));
        }
    }

    /// Through `main`: the dropped-option, overwritten-output and
    /// unqualified-timeout refusals are in analyze's preflight, before the
    /// workspace and every trial. `--run1-schedule` stops a `main` that passed
    /// them, with its own refusal right after that preflight, so each case
    /// asserts which refusal fired and that it was not that one (see the
    /// strict counter test below).
    #[test]
    fn trials_refuse_run_options_only_main_applies_before_their_workspace() {
        for (backend, extra, named) in [
            (None, "--namespace-only", "--namespace-only"),
            (None, "--lite", "--namespace-only"),
            (None, "--verify", "--verify"),
            (None, "--save-config=requested.config", "--save-config"),
            (None, "--summary-json=requested.summary", "--summary-json"),
            (
                Some("--backend=kvm"),
                "--timeout=3",
                "--timeout is not qualified",
            ),
            (
                None,
                "--record-networking=requested.trace",
                "refuse --record-networking=requested.trace",
            ),
            (
                None,
                "--strace-only",
                "refuse --strace-only in the run arguments",
            ),
            (
                None,
                "--no-sequentialize-threads",
                "refuse --no-sequentialize-threads in the run arguments",
            ),
            (
                None,
                "--network=host",
                "refuse --network=host in the run arguments",
            ),
            (
                None,
                "--no-namespace",
                "refuse --no-namespace in the run arguments",
            ),
            (
                None,
                "--gdbserver",
                "refuse --gdbserver in the run arguments",
            ),
            (
                None,
                "--record-preemptions-to=requested.preempts",
                "refuse --record-preemptions-to in the run arguments",
            ),
            (
                None,
                "--stacktrace-event=5,requested.stack",
                "refuse --stacktrace-event with a path in the run arguments",
            ),
            (
                None,
                "--preemption-stacktrace-log-file=requested.log",
                "refuse --preemption-stacktrace-log-file in the run arguments",
            ),
        ] {
            let mut argv = vec!["hermit"];
            argv.extend(backend);
            argv.extend([
                "analyze",
                "--run1-schedule=/nonexistent/schedule.json",
                "--",
                extra,
                "/bin/true",
            ]);
            let args = crate::Args::try_parse_from(&argv).unwrap();
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            let error = options.main(&args.global).unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{argv:?}: {error:#}"
            );
            let message = error.to_string();
            assert!(message.contains(named), "{argv:?}: {error:#}");
            assert!(
                !message.contains("does not implement"),
                "{argv:?}: main passed the preflight refusal: {error:#}"
            );
        }
    }

    /// Analyze's trials apply `--skid-margin`: its preflight records it as the
    /// skid margin of Reverie's per-process PMU configuration, so a later
    /// margin is refused. `main` refuses the unimplemented `--run1-schedule`
    /// right after that preflight, which stops it before any workspace. This
    /// relies on nextest running each test in a process of its own, as the
    /// validation DAG does.
    #[test]
    fn analyze_installs_the_trials_skid_margin() {
        let argv = [
            "hermit",
            "analyze",
            "--run1-schedule=/nonexistent/schedule.json",
            "--",
            "--skid-margin=4321",
            "/bin/true",
        ];
        let args = crate::Args::try_parse_from(argv).unwrap();
        let crate::Subcommand::Analyze(mut options) = args.command else {
            panic!("{argv:?} is not analyze")
        };
        // The refusal that follows the preflight, not an earlier one.
        let error = options.main(&args.global).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("hermit analyze does not implement --run1-schedule"),
            "main stopped before reaching the --run1-schedule refusal: {error:#}"
        );
        // A second margin is refused once one is recorded. Neither recording
        // reads the host's CPU, so this holds on a host Reverie has no PMU
        // profile for, such as a GitHub-hosted runner's Emerald Rapids.
        assert!(
            reverie_ptrace::set_skid_margin_override(1).is_err(),
            "analyze did not install the trials' --skid-margin"
        );
    }

    /// The printed reproducer carries `--timeout`, which every trial applies
    /// (`run_in_container` reads it), so the reproducer is bounded as the
    /// trial was.
    #[test]
    fn analyzer_reproducer_retains_the_trial_timeout() {
        use clap::CommandFactory;
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", "--", "--timeout=7", "/bin/true"]).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&options, "timeout-repro".to_owned()).unwrap();
        let repro = run.to_repro();
        assert_eq!(repro.matches(" --timeout=7").count(), 1, "{repro}");
        let matches = crate::Args::command()
            .try_get_matches_from(shell_words::split(&repro).unwrap())
            .unwrap();
        let args = crate::args_from_matches_with_clock(&matches, || {
            panic!("analyzer reproduction must retain its actual fixed input")
        })
        .unwrap();
        let crate::Subcommand::Run(mut parsed) = args.command else {
            panic!("expected run")
        };
        parsed.validate_args().unwrap();
        assert_eq!(parsed.to_string(), run.runopts.to_string());
    }

    /// Through `main`: the refusal must be wired into analyze's preflight,
    /// ahead of the workspace and every trial. `--run1-schedule` is not
    /// implemented and `main` refuses it right after that preflight, so an
    /// analyze that passed the counter refusal stops at that refusal instead,
    /// before starting anything; the message assertion below tells them apart.
    /// A trial with `--namespace-only` or `--lite` is refused earlier in that
    /// preflight, for a dropped option
    /// (`trials_refuse_run_options_only_main_applies_before_their_workspace`);
    /// `strict_trials_refuse_an_inexact_branch_counter` keeps it covered at
    /// the counter refusal itself.
    #[test]
    fn strict_analyze_refuses_an_inexact_branch_counter_before_its_workspace() {
        let argv = [
            "hermit",
            "analyze",
            "--run1-schedule=/nonexistent/schedule.json",
            "--",
            "--strict",
            "/bin/true",
        ];
        let args = crate::Args::try_parse_from(argv).unwrap();
        let crate::Subcommand::Analyze(mut options) = args.command else {
            panic!("{argv:?} is not analyze")
        };
        options.inexact_branch_counter = || Some("SpecLockMap is enabled".to_string());
        let error = options.main(&args.global).unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("SpecLockMap is enabled"),
            "{error:#}"
        );
    }

    /// `run` has no `--backend`, so `hermit --backend <BACKEND> analyze` is the
    /// only way to choose the trials' backend. The selection must pass the
    /// scope check and reach the options every trial is launched with: the
    /// `GlobalOpts` that `RunOpts::main` takes its backend from, the validated
    /// trial `RunOpts`, and the printed reproducer.
    /// `hermit --max-log-bytes=N analyze` caps its trials with the
    /// invocation's shared budget. A trial without --verbose/--selfcheck traces
    /// to hermit's stderr, so an exempt trial would put uncapped output on the
    /// public stream (review of https://github.com/rrnewton/hermit/pull/3686,
    /// round 1).
    #[test]
    fn analyze_trials_charge_the_invocations_log_budget() {
        let argv = ["hermit", "--max-log-bytes=4K", "analyze", "--", "/bin/true"];
        let mut args = crate::Args::try_parse_from(argv).unwrap();
        args.global.prepare_log_budget().unwrap();
        let crate::Subcommand::Analyze(mut options) = args.command else {
            panic!("{argv:?} is not analyze")
        };
        options.apply_global(&args.global);
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&options, "budget".to_owned()).unwrap();
        let trial = run.trial_global_opts();
        assert!(trial.log_file.is_none(), "this trial traces to stderr");
        assert_eq!(trial.max_log_bytes, Some(4096));
        let invocation = args.global.log_budget().expect("the invocation is capped");
        let charged = trial.log_budget().expect("the trial must be capped");
        assert!(
            charged.shares_counter_with(&invocation),
            "the trial must charge the invocation's counter, not a fresh one"
        );
    }

    #[test]
    fn global_backend_reaches_every_trial_run() {
        for flag in ["ptrace", "kvm"] {
            let argv = [
                "hermit".to_owned(),
                format!("--backend={flag}"),
                "analyze".to_owned(),
                "--".to_owned(),
                "/bin/true".to_owned(),
            ];
            let args = crate::Args::try_parse_from(&argv).unwrap();
            args.command
                .validate_backend_scope(args.global.backend)
                .unwrap_or_else(|error| panic!("{argv:?} must be in scope: {error:#}"));
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            let run = RunData::new_baseline(&options, "backend".to_owned())
                .unwrap_or_else(|error| panic!("{flag}: {error:#}"));
            assert_eq!(
                run.trial_global_opts().backend,
                args.global.backend,
                "{flag}"
            );
            assert_eq!(
                run.runopts.global_backend_arg(),
                format!(" --backend={flag}")
            );
            let repro = run.to_repro();
            assert!(
                repro.contains(&format!(" --backend={flag} run ")),
                "{flag}: reproducer does not select the backend: {repro}"
            );
        }
    }

    #[test]
    fn analyzer_reproducer_retains_actual_default_and_fractional_epoch() {
        use clap::CommandFactory;
        for epoch in ["2026-01-01T00:00:00Z", "2000-12-31T23:59:59.123456789Z"] {
            let mut options = AnalyzeOpts::try_parse_from([
                "analyze",
                "--",
                "--epoch",
                epoch,
                "--seed=71",
                "/bin/true",
            ])
            .unwrap();
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            let run = RunData::new_baseline(&options, "epoch-repro".to_owned()).unwrap();
            let repro = run.to_repro();
            assert_eq!(repro.matches("--epoch=").count(), 1, "{repro}");
            let matches = crate::Args::command()
                .try_get_matches_from(shell_words::split(&repro).unwrap())
                .unwrap();
            let args = crate::args_from_matches_with_clock(&matches, || {
                panic!("analyzer reproduction must retain its actual fixed input")
            })
            .unwrap();
            let crate::Subcommand::Run(parsed) = args.command else {
                panic!("expected run")
            };
            assert_eq!(
                parsed.det_opts.det_config.epoch,
                run.runopts.det_opts.det_config.epoch
            );
            assert_eq!(
                parsed.det_opts.det_config.seed,
                run.runopts.det_opts.det_config.seed
            );
        }
    }

    #[test]
    fn preemption_replay_preserves_selected_seeds_and_original_timer_policy() {
        for explicit_imprecise_timers in [false, true] {
            let mut args = vec![
                "analyze",
                "--selfcheck",
                "--search",
                "--imprecise-search",
                "--run1-seed=211",
                "--",
            ];
            if explicit_imprecise_timers {
                args.push("--imprecise-timers");
            }
            args.push("/bin/true");
            let mut options = AnalyzeOpts::try_parse_from(args).unwrap();
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            let mut selected = RunData::new_baseline(&options, "selected".to_owned()).unwrap();
            let selected_config = &mut selected.runopts.det_opts.det_config;
            selected_config.seed = 101;
            selected_config.rng_seed = Some(102);
            selected_config.fuzz_seed = Some(103);
            selected_config.sched_seed = Some(104);
            // This is the override launch_search applies, not a replay option.
            selected_config.imprecise_timers = true;
            let preempts = workspace.path().join("selected.preempts");
            let replay = RunData::new_run1_replay(&options, "replay".to_owned(), &selected.runopts)
                .unwrap()
                .with_preempts_path_in(preempts.clone())
                .with_preemption_recording();
            let config = &replay.runopts.det_opts.det_config;
            assert_eq!(config.seed, 101);
            assert_eq!(config.rng_seed, Some(102));
            assert_eq!(config.fuzz_seed, Some(103));
            assert_eq!(config.sched_seed, Some(104));
            assert_eq!(config.imprecise_timers, explicit_imprecise_timers);
            assert_eq!(config.replay_preemptions_from.as_ref(), Some(&preempts));
            assert_ne!(replay.runopts.save_config, selected.runopts.save_config);
            assert_ne!(replay.runopts.summary_json, selected.runopts.summary_json);
            assert_eq!(
                config.record_preemptions_to.as_ref(),
                Some(&workspace.path().join("replay_out.preempts")),
            );
        }
    }
}
