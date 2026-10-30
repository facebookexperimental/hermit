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
use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::bail;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::parser::ValueSource;
use colored::Colorize;
use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use detcore::types::SchedEvent;
use detcore_model::config::ExpiredWaitCharge;
use detcore_model::config::SleepRegistration;
use hermit::process::Bind;
use reverie::process::Output;
use tracing::metadata::LevelFilter;

use crate::app::analyze::consts::*;
use crate::app::analyze::stop;
use crate::app::analyze::types::AnalyzeOpts;
use crate::app::analyze::types::ContinuationTimeslice;
use crate::app::analyze::types::TRIAL_LOG_FACTOR;
use crate::app::analyze::types::TRIAL_LOG_FLOOR;
use crate::app::analyze::types::TRIAL_LOG_LEAST;
use crate::app::analyze::types::TRIAL_TIMEOUT_FACTOR;
use crate::app::analyze::types::TRIAL_TIMEOUT_FLOOR;
use crate::app::analyze::types::TrialLog;
use crate::app::analyze::types::TrialTimeout;
use crate::app::container::RunTimeoutMarker;
use crate::app::global_opts::GlobalOpts;
use crate::app::run::RunOpts;

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

    /// The level the run logs at ([`AnalyzeOpts::trial_log_level`]).
    log_level: LevelFilter,
    /// Where to write the guest log too (otherwise stderr).
    log_path: Option<PathBuf>,
    /// Whether `--selfcheck` may compare this run's log with another's: the
    /// target run, a `--search` run that may become it, a `--minimize` run
    /// that may be the last to match, and the target's replay. Under
    /// `--selfcheck` such a run logs at debug or above, into its file
    /// ([`RunData::logging`]).
    log_compared: bool,

    /// The input preemptions, if it has been read to memory.
    in_mem_preempts_in: Option<PreemptionRecord>,
    in_mem_sched_out: Option<PreemptionRecord>,

    is_a_match: Option<bool>,

    /// The `--trial-timeout` bound this run took, when it is shorter than the
    /// run arguments' own `--timeout` ([`AnalyzeOpts::trial_bound`]).
    trial_bound: Option<NonZeroU64>,
    /// How long the run took, once it finished.
    wall: Option<Duration>,
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

    /// Where analyze writes a preemption record for this run to replay
    /// ([`Self::with_preempts_in`]): the workspace, under the run's name.
    /// Never a path from the run arguments: a `--replay-preemptions-from`
    /// there names the user's own record, which a trial must not overwrite.
    pub fn preempts_path_in(&mut self) -> &Path {
        if self.preempts_path_in.is_none() {
            self.preempts_path_in = Some(self.root_path().with_extension(PREEMPTS_EXT));
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
            let record = PreemptionReader::new(path).load_all();
            // Trials spliced from this schedule make its jumps of global time.
            self.analyze_opts
                .time_skip_sources
                .lock()
                .unwrap()
                .remember(&record);
            self.in_mem_sched_out = Some(record);
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

    /// Where analyze writes a schedule for this run to replay (a bisection
    /// trial's, [`Self::with_schedule_replay`]): the workspace, under the
    /// run's name. Never a path from the run arguments: a
    /// `--replay-schedule-from` there names the user's own schedule, which a
    /// trial must not overwrite.
    pub fn sched_path_in(&mut self) -> &Path {
        if self.sched_path_in.is_none() {
            self.sched_path_in = Some(self.root_path().with_extension(PREEMPTS_EXT));
        }
        self.sched_path_in.as_ref().unwrap()
    }

    /// Remove the schedules a bisection trial wrote in the workspace (the one
    /// it replayed, the one it recorded, and that recording with event
    /// times), once its outcome and realized schedule are read, unless
    /// --keep-tmp keeps them. A bisection runs dozens of trials, each with
    /// schedules as large as the endpoints' (hundreds of MB for a few million
    /// events), which would otherwise stay until analyze exits. Only the
    /// workspace's own names are removed, never a path from the run arguments
    /// such as `--replay-schedule-from`, and a file the trial did not write
    /// (an unfinished trial records no schedule) is skipped.
    pub fn discard_schedules(&self) {
        if self.analyze_opts.keep_tmp {
            return;
        }
        for path in [
            self.root_path().with_extension(PREEMPTS_EXT),
            self.out_path().with_extension(PREEMPTS_EXT),
        ] {
            let _ = std::fs::remove_file(path);
        }
        self.discard_timed_recording();
    }

    /// Remove the recording with event times that a run makes and analyze
    /// rewrites without them ([`Self::post_process_events_file`]), unless
    /// --keep-tmp keeps it: analyze reads only the rewritten one.
    fn discard_timed_recording(&self) {
        if !self.analyze_opts.keep_tmp {
            let _ = std::fs::remove_file(self.timed_recording_path());
        }
    }

    /// Where a run records its schedule with event times, which analyze then
    /// rewrites without them to the path the run options name.
    fn timed_recording_path(&self) -> PathBuf {
        self.root_path().with_extension(PREEMPTS_EXT_PRESTRIPPED)
    }

    /// Convenience function
    pub fn sched_out_file_name(&mut self) -> String {
        self.sched_path_out()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    /// Known omissions cannot become an analysis endpoint or selfcheck proof.
    pub fn require_complete_output(&mut self) -> anyhow::Result<()> {
        let path = self.sched_path_out().to_path_buf();
        Self::require_complete_record(self.preempts_out(), &path)
    }

    pub(super) fn require_complete_record(
        record: &PreemptionRecord,
        path: &Path,
    ) -> anyhow::Result<()> {
        record.require_faithful_replay().map_err(|error| {
            anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
                "analysis cannot use {} as completed schedule evidence: {error:#}",
                path.display()
            ))
        })
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

    /// For a test that drives a phase with a launch it models: the completed
    /// observation `launch` makes, whether the run met the target, and the
    /// schedule it recorded.
    #[cfg(test)]
    pub(super) fn complete_for_test(
        &mut self,
        met: bool,
        recorded: &[SchedEvent],
    ) -> anyhow::Result<()> {
        self.is_a_match = Some(met);
        PreemptionRecord::from_sched_events(recorded.to_vec())
            .write_to_disk(self.sched_path_out())
            .map_err(anyhow::Error::msg)
    }

    /// Write a normalized copy of the output preemption record, for reading
    /// only, beside it, and return its path and contents. The run keeps its
    /// own record: normalization renumbers priorities across the scheduler's
    /// fixed ones, so the copy can replay another schedule.
    pub fn write_normalized_preempts(&mut self) -> (PathBuf, PreemptionRecord) {
        assert!(self.has_launched());
        let normalized_path = self.preempts_path_out().with_extension("normalized");
        let normalized = self.preempts_out().normalize();
        normalized
            .write_to_disk(&normalized_path)
            .expect("write of preempts file to succeed");
        (normalized_path, normalized)
    }

    /// The global options a trial run is launched with. Trials go through
    /// `RunOpts::run`, which dispatches on the trial `RunOpts`' own backend (set
    /// from `hermit --backend <BACKEND> analyze` by `get_raw_runopts`); this
    /// copy keeps the trial's global options consistent with it.
    fn trial_global_opts(&self) -> GlobalOpts {
        let (log, log_file) = self.logging();
        GlobalOpts {
            log: Some(log),
            log_file,
            backend: self.analyze_opts.backend,
            // A run analyze bounds in time also gets the log bound.
            log_file_limit: self
                .analyze_opts
                .trial_bound(&self.runopts)
                .and(self.analyze_opts.trial_log_limit()),
            // Every trial charges the invocation's ONE budget, as verify's
            // per-run logs do. A trial that writes no log file traces to
            // hermit's own stderr -- the public stream the cap exists to bound.
            max_log_bytes: self.analyze_opts.max_log_bytes,
            log_budget: self.analyze_opts.log_budget.clone(),
            log_file_handle: None,
            log_ring: None,
            log_ring_file: None,
            log_ring_handle: None,
            unsafe_ignore_host_seccomp: false,
        }
    }

    /// Execute the run. (Including setting up logging and temp dir binding.)
    ///
    /// Once a stop signal has arrived (`stop`), no run starts, and the run it
    /// ended reports the stop instead of its own outcome.
    pub fn launch(&mut self) -> anyhow::Result<()> {
        stop::check()?;
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
            let temp_path = self.timed_recording_path();
            self.runopts.det_opts.det_config.record_preemptions_to = Some(temp_path);
        }

        self.runopts.validate_args()?;
        // A trial starts through `RunOpts::run`, which applies none of the
        // preprocessing `RunOpts::main` does, so the trial runs the image its
        // reproducer names only if it is prepared here.
        self.runopts.prepare_trial_preprocessing()?;
        // A trial starts through `RunOpts::run`, which skips `RunOpts::main`'s
        // numbering of the guest's process IDs.
        self.runopts.prepare_trial_guest_ids()?;

        let repro_file = self.root_path().with_extension("repro");
        std::fs::write(repro_file, self.to_repro() + "\n")?;
        self.runopts.write_debug_options_dump()?;

        let started = Instant::now();
        let ran = self.runopts.run(&gopts, true);
        stop::check()?;
        let (_, output, terminal_outcome) = ran.map_err(|error| {
            self.run_error(self.name_the_trial_bound(error), gopts.log_file.as_deref())
        })?;
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
            self.discard_timed_recording();
            // Restore the setting:
            self.runopts.det_opts.det_config.record_preemptions_to = final_record_path;
        }

        self.is_a_match = Some(self.analyze_opts.trial_matches(&output, terminal_outcome)?);
        // Only a completed, successfully recorded run contributes a measurement.
        self.wall = Some(started.elapsed());

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

    /// Bound the run's wall-clock time ([`AnalyzeOpts::trial_bound`]) by the
    /// endpoint runs measured before it was created.
    fn apply_trial_bound(&mut self) {
        let Some(bound) = self.analyze_opts.trial_bound(&self.runopts) else {
            return;
        };
        if self
            .runopts
            .run_timeout()
            .is_none_or(|own| own.as_secs() > bound.get())
        {
            self.trial_bound = Some(bound);
        }
        self.runopts.bound_run_timeout(bound);
    }

    /// A run that `--trial-timeout`'s bound ended says so, and how to change
    /// the bound: the error itself names only the run's `--timeout`, which
    /// the user did not pass.
    fn name_the_trial_bound(&self, error: anyhow::Error) -> anyhow::Error {
        match self.trial_bound {
            Some(bound) if error.downcast_ref::<RunTimeoutMarker>().is_some() => {
                error.context(format!(
                    "{} reached hermit analyze's bound of {bound} s per run \
                     (--trial-timeout=SECONDS raises it, --trial-timeout=off lifts it); \
                     no program outcome was established",
                    self.runname
                ))
            }
            _ => error,
        }
    }

    /// How long the run took and how large its log file grew, once it
    /// finished: what an endpoint run contributes to the trials' bounds.
    pub fn measurement(&self) -> Option<(Duration, Option<u64>)> {
        if !self.has_launched() {
            return None;
        }
        let wall = self.wall?;
        let log_bytes = self
            .trial_global_opts()
            .log_file
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len());
        Some((wall, log_bytes))
    }

    /// This run's name in the workspace.
    pub fn name(&self) -> &str {
        &self.runname
    }

    /// Keep every failure typed. A policy refusal has no completed guest
    /// outcome, whether its reason arrived as context or was logged separately.
    fn run_error(&self, error: anyhow::Error, log: Option<&Path>) -> anyhow::Error {
        if error
            .downcast_ref::<crate::app::container::PolicyRefusal>()
            .is_none()
        {
            return error;
        }
        let diagnostics = log.map_or_else(
            || "Hermit's stderr above".to_owned(),
            |path| format!("the trial log {}", path.display()),
        );
        error.context(format!(
            "Hermit refused analysis run {}; no program outcome was established. \
             Inspect the refusal below and {diagnostics}; use a supported operation or \
             configuration before retrying.",
            self.runname,
        ))
    }

    /// Any post-processing that needs to be applied to a recorded file of SchedEvents
    pub fn post_process_events_file(sched_path: &Path, dest: &Path) -> anyhow::Result<()> {
        let mut recording = detcore::preemptions::boundary_recording::Recording::read(sched_path)?;
        recording.require_faithful_replay().map_err(|error| {
            anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
                "analysis trial at {} omitted a replay change point: {error:#}; \
                 the raw diagnostic record is retained and cannot certify an outcome",
                sched_path.display()
            ))
        })?;
        recording.strip_times();
        recording.write(dest)
    }

    /// The level this run logs at, and the file it logs into, if any. A run
    /// logs at the trials' level ([`AnalyzeOpts::trial_log_level`]), into a
    /// file of its own above warn. A run whose log `--selfcheck` compares
    /// logs at debug or above, into its file, whatever that level.
    fn logging(&self) -> (LevelFilter, Option<PathBuf>) {
        let compared = self.log_compared && self.analyze_opts.selfcheck;
        let level = if compared {
            self.log_level.max(LevelFilter::DEBUG)
        } else {
            self.log_level
        };
        let file = (compared || level > LevelFilter::WARN).then(|| {
            self.log_path
                .clone()
                .unwrap_or_else(|| self.root_path().with_extension(LOG_EXT))
        });
        (level, file)
    }

    /// Mark a run whose log `--selfcheck` may compare ([`RunData::logging`]).
    pub fn compared_by_selfcheck(mut self) -> Self {
        self.log_compared = true;
        self
    }

    /// A run with `runopts`, bounded by the endpoint runs measured so far
    /// ([`AnalyzeOpts::trial_bound`]).
    pub fn new(aopts: &AnalyzeOpts, runname: String, runopts: RunOpts) -> Self {
        let mut run = Self::new_unbounded(aopts, runname, runopts);
        run.apply_trial_bound();
        run
    }

    /// A run with `runopts` that no `--trial-timeout` bounds: the target run.
    fn new_unbounded(aopts: &AnalyzeOpts, runname: String, runopts: RunOpts) -> Self {
        let log_level = aopts.trial_log_level();
        let mut rd = RunData {
            runname,
            analyze_opts: aopts.clone(),
            runopts,
            preempts_path_in: None,
            sched_path_in: None,
            sched_path_out: None,
            log_path: None,
            log_compared: false,
            in_mem_preempts_in: None,
            in_mem_sched_out: None,
            is_a_match: None,
            log_level,
            trial_bound: None,
            wall: None,
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

    /// Create the initial target attempt. `auto` does not bound it: the
    /// trials' bound derives from it; the run arguments' own timeout still
    /// applies. With --run1-preemptions it replays a copy of the record in the
    /// workspace, because the guest's container does not see the host's
    /// /tmp, where records often are; that replay can hang like any other,
    /// so an explicit --trial-timeout bounds it.
    pub fn new_run1_target(aopts: &AnalyzeOpts, runname: String) -> anyhow::Result<Self> {
        let ro = Self::get_run1_runopts(aopts)?;
        let mut run = Self::new_unbounded(aopts, runname, ro);
        if let Some(record) = &aopts.run1_preemptions {
            let copy = run.root_path().with_extension(PREEMPTS_EXT);
            std::fs::copy(record, &copy).with_context(|| {
                format!(
                    "cannot copy --run1-preemptions {} into the workspace",
                    record.display()
                )
            })?;
            run = run.with_preempts_path_in(copy);
            run.apply_trial_bound();
        }
        Ok(run.compared_by_selfcheck())
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
        replay.apply_trial_bound();
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

        // Trials may differ only in their schedule. `--seed` also seeds what the guest reads
        // as randomness (getrandom, /dev/urandom) and syscall fuzzing, so a run2 seed or a
        // schedule replay at the default seed would hand the guest other inputs, and no
        // schedule could reproduce the target. Pin both to the target's seed.
        let config = &mut ro.det_opts.det_config;
        let target_seed = aopts.run1_seed.unwrap_or(config.seed);
        config.rng_seed.get_or_insert(target_seed);
        config.fuzz_seed.get_or_insert(target_seed);

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

    /// Trials write their records into the workspace from inside the
    /// container, whose `/tmp` is private: a workspace under host `/tmp` is
    /// bound into it. One elsewhere (`--tmp-dir`) is visible as it is, and a
    /// bind of it would only make every trial warn that the bind has no
    /// effect (`hermit run` honors binds under `/tmp` alone). Where the
    /// workspace is is decided on its canonical path, so a relative path or a
    /// symlink into `/tmp` is bound, and one under `/tmp` that leads out of it
    /// is not.
    fn runopts_add_binds(aopts: &AnalyzeOpts, runopts: &mut RunOpts) -> anyhow::Result<()> {
        let workspace = aopts.get_tmp()?;
        let workspace = std::fs::canonicalize(workspace)
            .or_else(|_| std::path::absolute(workspace))
            .map_err(|error| {
                anyhow::anyhow!(
                    "cannot resolve the workspace {}: {error}",
                    workspace.display()
                )
            })?;
        if workspace.starts_with("/tmp") {
            runopts
                .bind
                .push(Bind::from_str(workspace.to_str().unwrap())?);
        }
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
        let matches = RunOpts::command()
            .try_get_matches_from(run_cmd.iter())
            .unwrap_or_else(|error| {
                if crate::app::misplaced_backend_argument(&error).is_some() {
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
        let mut runopts = RunOpts::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
        runopts.capture_max_timeslice_source(&matches);
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
        // --run1-preemptions: `new_run1_target` replays a workspace copy.
        if let Some(seed) = aopts.run1_seed {
            ro.det_opts.det_config.seed = seed;
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
        let log_level = aopts.trial_log_level();
        RunData {
            runname,
            analyze_opts: aopts.clone(),
            runopts,
            preempts_path_in: None,
            sched_path_in: None,
            sched_path_out: Some(preempts_path),
            log_path: Some(log_path),
            // The last matching minimize run, whose log --selfcheck compares.
            log_compared: true,
            log_level,
            in_mem_sched_out: Some(in_mem_preempts),
            in_mem_preempts_in: None,
            // Invariant: minimize should always return an on-target configuration:
            is_a_match: Some(true),
            trial_bound: None,
            wall: None,
        }
    }

    /// Another fake run that stores a result without actually launching anything.
    pub fn from_schedule_trace(
        aopts: &AnalyzeOpts,
        runname: String,
        runopts: RunOpts,
        sched_path: PathBuf,
    ) -> Self {
        let log_level = aopts.trial_log_level();
        RunData {
            runname,
            analyze_opts: aopts.clone(),
            runopts,
            preempts_path_in: None,
            sched_path_in: None,
            sched_path_out: Some(sched_path),
            log_path: None,
            log_compared: false,
            log_level,
            in_mem_sched_out: None,
            in_mem_preempts_in: None,
            // Don't claim that it was run:
            is_a_match: None,
            trial_bound: None,
            wall: None,
        }
    }

    /// Replay the preemption record at `path`, a recording: a replay that
    /// reaches a recorded slice end already passed stops, as any replay
    /// that diverges does.
    pub fn with_preempts_path_in(mut self, path: PathBuf) -> Self {
        self.runopts.det_opts.det_config.replay_preemptions_from = Some(path);
        self
    }

    /// Replay the preemption record at `path`, one analyze edited
    /// (minimization and the baseline search knock preemptions out of a
    /// recording) or one a replay of such a record met the target with. A
    /// thread can then reach recorded slices that have already ended, which
    /// the replay skips (`--replay-edited-preemptions`).
    pub fn with_edited_preempts_path_in(self, path: PathBuf) -> Self {
        let mut run = self.with_preempts_path_in(path);
        run.runopts.det_opts.det_config.replay_edited_preemptions = true;
        run
    }

    /// Replay `pr`, a record analyze edited
    /// ([`Self::with_edited_preempts_path_in`]).
    pub fn with_preempts_in(mut self, pr: PreemptionRecord) -> Self {
        // Derived from a trial's own record, which states the trials' guest
        // IDs already; stated again, as every record written for a trial is.
        let pr = match crate::app::run::resolved_trial_guest_ids() {
            Some(ids) => pr.with_guest_ids(ids),
            None => pr,
        };
        let path = self.preempts_path_in().to_path_buf();
        pr.write_to_disk(&path)
            .expect("write of preempts file to succeed");
        self.in_mem_preempts_in = Some(pr);
        self.with_edited_preempts_path_in(path)
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
        // A schedule-replay trial follows its guide, and its recording
        // must hold the priorities it takes: it records logical
        // priorities (no chaos draw, no scheduling heuristic), so the
        // record it writes is complete rather than omitting every
        // priority decision of the replay. The search runs, which do
        // not replay a schedule, keep the baseline's chaos.
        self.runopts.det_opts.det_config.chaos = false;
        self.runopts.det_opts.det_config.sched_heuristic =
            detcore_model::config::SchedHeuristic::None;
        self
    }

    /// The run's reproducer: a shell line that changes to the directory the
    /// trial ran in, then runs [`Self::repro_command`].
    ///
    /// A trial without `--workdir` gives its guest Hermit's working
    /// directory, which is analyze's own; a run of the bare command from
    /// another directory would start its guest there instead
    /// (<https://github.com/rrnewton/hermit/issues/4812>). The `cd` keeps that
    /// inherited directory rather than passing it as `--workdir`, which
    /// would name a different one: `--workdir` resolves after the guest's
    /// mounts, so a directory under the guest's private `/tmp` does not
    /// exist there. The `cd` also holds Hermit's own relative paths (an
    /// `--image` or `--bind` source) to the trial's, so it is written even
    /// with `--workdir`. Only when analyze's directory cannot be named
    /// (removed, or not UTF-8) is the line the bare command.
    pub fn to_repro(&self) -> String {
        let dir = std::env::current_dir().ok();
        reproducer_line(dir.as_deref().and_then(Path::to_str), self.repro_command())
    }

    /// The `hermit ... run ...` command that reproduces this run, without
    /// the working directory it needs ([`Self::to_repro`]), with the global
    /// options that set its logging: none for the default warn level to
    /// stderr.
    pub fn repro_command(&self) -> String {
        let logging = match self.logging() {
            (level, Some(path)) => format!(" --log={level} --log-file={}", path.display()),
            (level, None) if level != LevelFilter::WARN => format!(" --log={level}"),
            (_, None) => String::new(),
        };
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

/// `cd <dir> && <command>` with `dir` quoted for the shell, or the bare
/// command when there is no directory to name ([`RunData::to_repro`]).
fn reproducer_line(dir: Option<&str>, command: String) -> String {
    match dir {
        Some(dir) => format!("cd {} && {command}", shell_words::quote(dir)),
        None => command,
    }
}

impl AnalyzeOpts {
    /// The wall-clock bound, in whole seconds, on a run with `trial`'s
    /// options (`--trial-timeout`): none where `--timeout` cannot be
    /// enforced, and with `auto` max(60 s, 10 times the slowest endpoint run
    /// measured so far), none before one was.
    pub fn trial_bound(&self, trial: &RunOpts) -> Option<NonZeroU64> {
        if !trial.accepts_run_timeout() {
            return None;
        }
        match self.trial_timeout {
            TrialTimeout::Off => None,
            TrialTimeout::Seconds(seconds) => Some(seconds),
            TrialTimeout::Auto => {
                let bound = self
                    .endpoints
                    .slowest?
                    .saturating_mul(TRIAL_TIMEOUT_FACTOR)
                    .max(TRIAL_TIMEOUT_FLOOR);
                NonZeroU64::new(
                    bound
                        .as_secs()
                        .saturating_add(u64::from(bound.subsec_nanos() > 0)),
                )
            }
        }
    }

    /// The cap on a trial's log file while trials are bounded: max(64 MiB,
    /// 10 times the largest endpoint log), once an endpoint run has written
    /// one.
    pub fn trial_log_limit(&self) -> Option<u64> {
        if self.trial_timeout == TrialTimeout::Off {
            return None;
        }
        let largest = self.endpoints.largest_log?;
        Some(
            largest
                .saturating_mul(TRIAL_LOG_FACTOR)
                .max(TRIAL_LOG_FLOOR),
        )
    }

    /// The level the runs analyze starts log at: `--trial-log`, else
    /// `--guest-log`, else warn, and never below errors ([`TRIAL_LOG_LEAST`]).
    /// `--verbose` does not raise it.
    pub fn trial_log_level(&self) -> LevelFilter {
        self.trial_log
            .map(TrialLog::level)
            .or(self.guest_log)
            .unwrap_or(LevelFilter::WARN)
            .max(TRIAL_LOG_LEAST)
    }

    /// The option that makes every run write its log into a file of its own
    /// (a level above warn), if one does.
    pub fn trial_log_files_option(&self) -> Option<&'static str> {
        if self.trial_log_level() <= LevelFilter::WARN {
            None
        } else if self.trial_log.is_some() {
            Some("--trial-log")
        } else if self.guest_log_from_env {
            Some("--guest-log (HERMIT_LOG)")
        } else {
            Some("--guest-log")
        }
    }

    /// Record a finished endpoint `run`, and say what bound the runs after it
    /// now get, when that changed.
    pub fn record_endpoint(&mut self, run: &RunData) {
        let Some((wall, log_bytes)) = run.measurement() else {
            return;
        };
        let trial = RunData::get_raw_runopts(self);
        let first = self.endpoints.slowest.is_none();
        let before = self.trial_bound(&trial);
        if !self.endpoints.record(wall, log_bytes) || self.trial_timeout != TrialTimeout::Auto {
            return;
        }
        let Some(bound) = self.trial_bound(&trial) else {
            if !first {
                return;
            }
            eprintln!(
                ":: {} the runs' backend does not enforce hermit run --timeout, so analyze \
                 does not bound them (--trial-timeout). Bound the whole analysis from outside \
                 instead, for example with timeout(1).",
                "WARNING:".yellow().bold()
            );
            return;
        };
        if before == Some(bound) {
            return;
        }
        let shorter = trial
            .run_timeout()
            .filter(|own| own.as_secs() < bound.get())
            .map_or(String::new(), |own| {
                format!(
                    "; the run arguments' shorter --timeout={} applies instead",
                    own.as_secs()
                )
            });
        eprintln!(
            ":: Runs from here on are bounded at {bound} s of wall-clock time: max(60 s, 10 x \
             the slowest endpoint run, {} at {:.1} s) (--trial-timeout{shorter}).",
            run.name(),
            wall.as_secs_f64(),
        );
    }

    /// Refuses (exit 122) an explicit `--trial-timeout=SECONDS` where the
    /// trials' backend does not enforce `hermit run --timeout`
    /// (`RunOpts::ensure_timeout_supported`), before any workspace or trial
    /// exists: the bound could not be kept.
    pub fn refuse_unenforceable_trial_timeout(&self) -> anyhow::Result<()> {
        let trial = RunData::get_raw_runopts(self);
        match self.trial_timeout {
            TrialTimeout::Seconds(seconds) if !trial.accepts_run_timeout() => Err(
                anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
                    "--trial-timeout={seconds} cannot be enforced: the trials run on a backend \
                     on which `hermit run --timeout` is not qualified. Use --trial-timeout=off \
                     and an outer bound (see docs/TIMEOUT_LADDER.md)."
                )),
            ),
            _ => Ok(()),
        }
    }

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
    /// (<https://github.com/rrnewton/hermit/issues/3810>). Unlike `run`, a trial
    /// with `--namespace-only` is not exempt: trials never reach the
    /// namespace-only launcher, so they still run on ptrace.
    pub fn refuse_strict_with_inexact_branch_counter(&self) -> anyhow::Result<()> {
        RunData::get_raw_runopts(self).refuse_strict_trial_with_inexact_branch_counter()
    }

    /// The warning for trials that go on without `--strict` on a
    /// retired-branch counter that fails Reverie's validation, wherever
    /// [`Self::refuse_strict_with_inexact_branch_counter`] refuses `--strict`
    /// (`RunOpts::trial_inexact_branch_counter_warning`): a trial can then
    /// disagree with another for a hardware reason, and a seed, preemption
    /// record or schedule that analyze or bisect reports need not reproduce
    /// its outcome.
    pub fn trial_branch_counter_warning(&self) -> Option<String> {
        RunData::get_raw_runopts(self).trial_inexact_branch_counter_warning()
    }

    /// The branch-counter check `analyze` and `bisect` make before any
    /// workspace or trial, as `RunOpts::admit_branch_counter` does for a run:
    /// [`Self::refuse_strict_with_inexact_branch_counter`], then
    /// [`Self::trial_branch_counter_warning`] passed to `warn` (stderr in
    /// `main`), once for all the trials, and kept in
    /// `AnalyzeOpts::branch_counter_warning` for the final report.
    pub fn admit_branch_counter(&mut self, warn: &mut dyn FnMut(&str)) -> anyhow::Result<()> {
        self.refuse_strict_with_inexact_branch_counter()?;
        let warning = self.trial_branch_counter_warning();
        if let Some(warning) = &warning {
            warn(warning);
        }
        self.branch_counter_warning = warning.map(|line| line.trim_end().to_owned());
        Ok(())
    }

    /// The notice `hermit run` writes for a small `--max-timeslice`
    /// (`RunOpts::small_max_timeslice_notice`), for the run arguments every
    /// trial shares. Trials go straight to `RunOpts::run`, so `analyze` and
    /// `bisect` write it once here, after the trials' PMU configuration is
    /// installed, rather than once per trial. `skid_margin` is normally
    /// `run::installed_skid_margin`.
    pub fn small_trial_max_timeslice_notice(&self, skid_margin: fn() -> u64) -> Option<String> {
        RunData::get_raw_runopts(self).small_trial_max_timeslice_notice(skid_margin)
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

    /// Refuse, before any trial, input recordings that number their guests
    /// apart (`GuestIds::numbers_like`). Every trial of one analysis or
    /// bisection takes the same guest IDs (`RunOpts::prepare_trial_guest_ids`),
    /// so a trial replaying one of them could not follow it. Two
    /// `--no-namespace` runs choose their bases independently unless both
    /// were given the same `--pid-base`.
    pub fn refuse_recordings_numbered_apart(recordings: &[(&str, &Path)]) -> anyhow::Result<()> {
        let mut first: Option<(String, detcore_model::pid::GuestIds)> = None;
        for (option, path) in recordings {
            let ids =
                detcore::preemptions::read_recorded_guest_ids(path).map_err(anyhow::Error::msg)?;
            let source = format!("{option} {}", path.display());
            match &first {
                None => first = Some((source, ids)),
                Some((first_source, first_ids)) if !first_ids.numbers_like(&ids) => {
                    // A policy refusal (exit 122), as the epoch's and the
                    // pipe-wake latency's are, not a CLI error.
                    return Err(
                        anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
                            "{first_source} was recorded with guest process IDs {first_ids}, and \
                             {source} with {ids}. Every trial numbers its guest alike, so no \
                             trial could follow both records. Record both runs under \
                             --no-namespace with the same --pid-base"
                        )),
                    );
                }
                Some(_) => {}
            }
        }
        Ok(())
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
    /// times too (<https://github.com/rrnewton/hermit/issues/3875>).
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
                    return Err(
                        anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
                            "{} was recorded under the virtual-time epoch {} and {source} \
                             under {}. Each record's times are measured from its own epoch, \
                             so no single replay epoch reproduces both. Record both with the \
                             same --epoch.",
                            sources[0],
                            first.to_rfc3339(),
                            epoch.to_rfc3339(),
                        )),
                    );
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
            anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
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

    /// Run every trial under the `--max-timeslice` the recordings state their
    /// run drew under `--chaos-timeslice-draw=run`
    /// (`PreemptionRecord::chaos_timeslice`), as `adopt_recorded_epoch` runs
    /// them from the recorded epoch. A trial replays schedules analyze writes,
    /// which state no timeslice, so a replay that took it from its record
    /// would not find it there: the timeslice is passed as an explicit
    /// `--max-timeslice` instead.
    ///
    /// The run arguments may not draw a timeslice per run
    /// (`--chaos-timeslice-draw=run`): each trial would draw its own from its
    /// own seed, and the trials must share one. A per-slice range
    /// (`--chaos-timeslice-range` alone) is fine: every trial gets its MAX. Recordings that state different timeslices, or an explicit
    /// `--max-timeslice` that differs from theirs, are refused before any
    /// trial.
    pub fn adopt_recorded_timeslice(&mut self, recordings: &[(&str, &Path)]) -> anyhow::Result<()> {
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
        let refuse = |message: String| {
            Err(anyhow::Error::new(crate::app::container::PolicyRefusal).context(message))
        };
        let config = &parsed.det_opts.det_config;
        if config.chaos_timeslice_draw == detcore_model::config::ChaosTimesliceDraw::Run {
            let range = config
                .chaos_timeslice_range
                .unwrap_or(detcore_model::config::RECOMMENDED_CHAOS_TIMESLICE_RANGE);
            return refuse(format!(
                "`hermit analyze` and `hermit bisect` cannot take --chaos-timeslice-range={range} \
                 --chaos-timeslice-draw=run in their run arguments: each trial would draw another \
                 --max-timeslice from its own seed, and every trial must run under one. Pass the \
                 timeslice of the run being analyzed instead, --max-timeslice=<ns>, as its \
                 --summary-json reports it (preemption.max_timeslice_ns); a recording of it given \
                 here supplies it."
            ));
        }
        let mut recorded: Option<(u64, String)> = None;
        for (option, path) in recordings {
            let Some(timeslice) = detcore::preemptions::read_recorded_chaos_timeslice(path)
                .map_err(anyhow::Error::msg)?
            else {
                continue;
            };
            let source = format!("{option} {}", path.display());
            match &recorded {
                Some((first, first_source)) if *first != timeslice => {
                    return refuse(format!(
                        "{first_source} was recorded under a drawn --max-timeslice of {first} ns \
                         and {source} under {timeslice} ns. Every trial runs under one \
                         timeslice, so no single one replays both. Record both under the same \
                         --max-timeslice."
                    ));
                }
                Some(_) => {}
                None => recorded = Some((timeslice, source)),
            }
        }
        let Some((timeslice, source)) = recorded else {
            return Ok(());
        };
        if config.chaos_timeslice_range.is_some() {
            // Every trial replays a record or runs under the range's MAX; a
            // drawn timeslice cannot also be given beside the range.
            return refuse(format!(
                "{source} was recorded under a --max-timeslice of {timeslice} ns drawn by \
                 --chaos-timeslice-draw=run, and the run arguments give --chaos-timeslice-range, \
                 which would run the trials under another. Remove --chaos-timeslice-range from \
                 the run arguments to run them under the recorded timeslice."
            ));
        }
        if matches.value_source("max_timeslice") == Some(ValueSource::DefaultValue) {
            self.run_arg.push(format!("--max-timeslice={timeslice}"));
            return Ok(());
        }
        let explicit = config.max_timeslice;
        if explicit.map(std::num::NonZeroU64::get) == Some(timeslice) {
            return Ok(());
        }
        refuse(format!(
            "the run arguments' --max-timeslice ({}) differs from the {timeslice} ns that {source} \
             was recorded under, drawn by --chaos-timeslice-draw=run. Omit --max-timeslice to run \
             the trials under the recorded one, or pass --max-timeslice={timeslice}.",
            explicit.map_or_else(|| "disabled".to_owned(), |maximum| format!("{maximum} ns"))
        ))
    }

    /// Refuses (exit 122) a `--continuation-timeslice` that would change the
    /// replayed prefix, before any workspace or trial: one shorter than the
    /// run's `--max-timeslice`, which also ends the prefix's slices that its
    /// schedule does not end, or one for a run without PMU timeslices, whose
    /// clock it would change.
    pub fn refuse_unsound_continuation_timeslice(&self) -> anyhow::Result<()> {
        self.refuse_unsound_continuation_for(self.trial_max_timeslice()?)
    }

    /// Recheck against the target record once it is available, before any
    /// prefix trial can apply an explicitly selected continuation.
    pub(super) fn refuse_unsound_continuation_for(
        &self,
        run: detcore_model::config::MaybeTimeslice,
    ) -> anyhow::Result<()> {
        let ContinuationTimeslice::Nanos(nanos) = self.continuation_timeslice else {
            return Ok(());
        };
        let reason = match run {
            None => "the run arguments disable PMU timeslices, and a continuation timeslice \
                     would turn them on for the replayed prefix too"
                .to_owned(),
            Some(run) if nanos < run => format!(
                "it is shorter than the run's --max-timeslice={run}, and the trials' timeslice \
                 also ends the replayed prefix's slices that its schedule does not end, so it \
                 would change the prefix"
            ),
            Some(_) => return Ok(()),
        };
        Err(
            anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
                "--continuation-timeslice={nanos} is refused: {reason}. Omit it (auto), or pass \
                 a timeslice at least as long as the run's own."
            )),
        )
    }

    /// The non-chaos replay trials' effective maximum, after recorded-value
    /// adoption and the same range/seed resolution used at launch (`None`
    /// for disabled).
    pub fn trial_max_timeslice(&self) -> anyhow::Result<detcore_model::config::MaybeTimeslice> {
        RunData::get_raw_runopts(self).analysis_replay_max_timeslice()
    }

    /// The trials' `--pipe-wake-latency`, from their run arguments (`None`
    /// for off).
    pub fn trial_pipe_wake_latency(&self) -> Option<u64> {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .pipe_wake_latency
    }

    /// Whether the trials keep the signal-state copy (off with
    /// `--no-signal-state-copy` in their run arguments).
    pub fn trial_signal_state_copy(&self) -> bool {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .signal_state_copy
    }

    /// The trials' `--expired-wait-charge`, from their run arguments, where
    /// [`Self::adopt_expired_wait_charge`] puts it; `each` before then, the
    /// rule of the `--chaos` runs analyze makes.
    pub fn trial_expired_wait_charge(&self) -> ExpiredWaitCharge {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .expired_wait_charge
            .unwrap_or(ExpiredWaitCharge::Each)
    }

    /// Run every trial under one `--expired-wait-charge`: the run arguments'
    /// own, or else the rule of the `target` recording (`--run1-preemptions`
    /// for analyze, `--bad` for bisect), or else `each`, the rule of the
    /// `--chaos` target run analyze makes. A search mixes the target's
    /// schedule with a baseline's recorded without `--chaos`, so the two must
    /// not charge expired waits differently, and a failure is searched under
    /// the rule that found it. The value is added to the run arguments, as
    /// [`Self::adopt_recorded_epoch`] adds the epoch, so trial commands and
    /// reproducers carry it; recordings under another rule are then refused
    /// ([`Self::refuse_recordings_under_another_pipe_wake_latency`]).
    pub fn adopt_expired_wait_charge(&mut self, target: Option<&Path>) -> anyhow::Result<()> {
        if RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .expired_wait_charge
            .is_some()
        {
            return Ok(());
        }
        let once = match target {
            Some(path) => detcore::preemptions::read_recorded_expired_wait_runs_charged_once(path)
                .map_err(anyhow::Error::msg)?,
            None => false,
        };
        self.run_arg.push(format!(
            "--expired-wait-charge={}",
            ExpiredWaitCharge::from_charged_once(once).as_str()
        ));
        Ok(())
    }

    /// The trials' `--sleep-registration`, from their run arguments, where
    /// [`Self::adopt_sleep_registration`] puts it; `when-chosen` before then,
    /// the rule of the `--chaos` runs analyze makes.
    pub fn trial_sleep_registration(&self) -> SleepRegistration {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .sleep_registration
            .unwrap_or(SleepRegistration::WhenChosen)
    }

    /// Run every trial under one `--sleep-registration`: the run arguments'
    /// own, or else the rule of the `target` recording (`--run1-preemptions`
    /// for analyze, `--bad` for bisect), or else `when-chosen`, the rule of
    /// the `--chaos` target run analyze makes. A search mixes the target's
    /// schedule with a baseline's recorded without `--chaos`, so the two must
    /// not register sleeps differently, and a failure is searched under the
    /// rule that found it. The value is added to the run arguments, as
    /// [`Self::adopt_recorded_epoch`] adds the epoch, so trial commands and
    /// reproducers carry it; recordings under another rule are then refused
    /// ([`Self::refuse_recordings_under_another_pipe_wake_latency`]).
    pub fn adopt_sleep_registration(&mut self, target: Option<&Path>) -> anyhow::Result<()> {
        if RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .sleep_registration
            .is_some()
        {
            return Ok(());
        }
        let first_pass = match target {
            Some(path) => {
                detcore::preemptions::read_recorded_queued_sleeps_registered_at_first_pass(path)
                    .map_err(anyhow::Error::msg)?
            }
            None => false,
        };
        self.run_arg.push(format!(
            "--sleep-registration={}",
            SleepRegistration::from_registered_at_first_pass(first_pass).as_str()
        ));
        Ok(())
    }

    /// `record` stamped with the trials' `--pipe-wake-latency`, signal-state
    /// copy setting, `--expired-wait-charge` and `--sleep-registration`, as
    /// every record analyze writes must be: a record that omitted one would
    /// be accepted when replayed under another value
    /// Whether the trials rewrite exclusive loops in the guest's memory.
    pub fn trial_rewrite_exclusives_in_memory(&self) -> bool {
        RunData::get_raw_runopts(self).rewrites_exclusives_in_memory()
    }

    /// The post-crash bound in effect in the trials
    /// (`Config::post_crash_timeslice_in_effect`; `None` for off).
    pub fn trial_post_crash_timeslice(&self) -> Option<u64> {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .post_crash_timeslice_in_effect()
            .map(std::num::NonZeroU64::get)
    }

    /// `record` stamped with the trials' `--pipe-wake-latency`, signal-state
    /// copy setting, `--expired-wait-charge`, in-memory exclusive rewrite and
    /// post-crash bound, as every record analyze writes must be: a record that
    /// omitted one would be accepted, or refused, when replayed under another
    /// value
    /// (<https://github.com/rrnewton/hermit/issues/4380>).
    pub fn stamp_trial_record(&self, record: PreemptionRecord) -> PreemptionRecord {
        let record = record
            .with_pipe_wake_latency(self.trial_pipe_wake_latency())
            .with_signal_state_copy(Some(self.trial_signal_state_copy()))
            .with_expired_wait_runs_charged_once(
                self.trial_expired_wait_charge() == ExpiredWaitCharge::Once,
            )
            .with_cap_preemption_charge(self.trial_cap_preemption_charge())
            .with_queued_sleeps_registered_at_first_pass(
                self.trial_sleep_registration() == SleepRegistration::FirstPass,
            )
            .with_rewrite_exclusives_in_memory(self.trial_rewrite_exclusives_in_memory())
            .with_post_crash_timeslice(self.trial_post_crash_timeslice());
        // Under `--no-namespace`, the guest IDs every trial takes, resolved
        // before the first (`prepare_trial_guest_ids`): a record without them
        // reads as numbered by a PID namespace, which no trial can replay.
        match crate::app::run::resolved_trial_guest_ids() {
            Some(ids) => record.with_guest_ids(ids),
            None => record,
        }
    }

    /// Whether the trials cap preemption charges (`--cap-preemption-charge`
    /// in their run arguments).
    pub fn trial_cap_preemption_charge(&self) -> bool {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .cap_preemption_charge
    }

    /// Number every trial's guest before the first trial, from the input
    /// `recordings` (`RunOpts::prepare_trial_guest_ids_for_inputs`), so that
    /// every record written for a trial states it (`Self::stamp_trial_record`).
    /// A refusal here comes before any trial.
    pub fn prepare_trial_guest_ids(&self, recordings: &[(&str, &Path)]) -> anyhow::Result<()> {
        RunData::get_raw_runopts(self).prepare_trial_guest_ids_for_inputs(recordings)
    }

    /// Write `events`, a schedule a search built from the schedules of
    /// `sources`, for a trial to replay, stamped
    /// ([`Self::stamp_trial_record`]), with the callback checkpoints of the
    /// part it shares with one of them ([`trial_record_from`]); with no
    /// sources, the events alone.
    pub fn write_trial_schedule_from(
        &self,
        events: Vec<SchedEvent>,
        sources: &[&PreemptionRecord],
        path: &Path,
    ) -> anyhow::Result<()> {
        self.write_trial_record(trial_record_from(events, sources), path)
    }

    /// Write `record` for a trial to replay, stamped
    /// ([`Self::stamp_trial_record`]). The record carries the jumps of global
    /// time of the recorded schedule whose event prefix its events repeat
    /// furthest (`crate::app::time_skip_carry`), so its replay jumps where that
    /// recording did. All sources must share one timing format; mixing
    /// formats cannot preserve their different switch-in charging rules.
    /// A record that notes its background starts (a prefix of a whole
    /// recorded schedule) keeps its own format and its own jumps, from the
    /// same recording as its starts, never another source's jumps beside them.
    /// An incompatible format or invalid record is refused before writing.
    pub fn write_trial_record(&self, record: PreemptionRecord, path: &Path) -> anyhow::Result<()> {
        let (skips, format, identity) = {
            let sources = self.time_skip_sources.lock().unwrap();
            (
                sources.carried(record.schedevents()),
                sources.trial_format()?,
                sources.trial_identity_metadata(record.schedevents())?,
            )
        };
        let (guest_ids, reserved_input) = identity.unwrap_or_else(|| {
            (
                record.guest_ids().unwrap_or_default(),
                record.reserved_input().clone(),
            )
        });
        if matches!(
            record.guest_ids(),
            Some(detcore_model::pid::GuestIds::Chosen { .. })
        ) {
            record
                .reserved_input()
                .require_same_for_chosen(&reserved_input)?;
        }
        // A trial that keeps a source's noted background starts (a prefix of
        // its whole schedule) is one recording's whole history: it keeps that
        // source's format and its own jumps, from the same recording as its
        // starts, whichever source the cache would pick for the same events.
        // Any other takes the sources' shared format, which lies below the
        // background-starts format (`TimeSkipSources::remember`), and the
        // cache's jumps.
        let (record, skips) = if record.notes_background_starts() {
            let own = record.time_skips().unwrap_or_default().to_vec();
            (record, own)
        } else {
            (record.with_format(format), skips)
        };
        let record = self.stamp_trial_record(
            record
                .with_time_skips(skips)
                .with_guest_ids(guest_ids)
                .with_reserved_input(reserved_input),
        );
        record.validate().map_err(anyhow::Error::msg)?;
        record.write_to_disk(path).map_err(anyhow::Error::msg)
    }

    /// Refuse, before any trial, recordings made under another
    /// `--pipe-wake-latency` than the trials' own, including a different
    /// latency (<https://github.com/rrnewton/hermit/issues/4380>). Trials start
    /// through `RunOpts::run`, which skips `hermit run`'s own check, so the
    /// check is made here, as `adopt_recorded_epoch` makes the epoch's. The
    /// run arguments name no record to replay: `refuse_outputs_trials_overwrite`
    /// refuses one first. A record without the field was made with the option
    /// off. The same recordings are checked for the signal-state copy setting
    /// (`--no-signal-state-copy`), which a record without that field does not
    /// state, so it is accepted under either, and for the trials'
    /// `--expired-wait-charge` ([`Self::adopt_expired_wait_charge`]), which a
    /// record without that field states as `each`, and for the trials'
    /// `--sleep-registration` ([`Self::adopt_sleep_registration`]), which a
    /// record without that field states as `when-chosen`.
    /// record without that field states as `each`, and for the in-memory
    /// exclusive-loop rewrite, which a record without that field was made
    /// without.
    pub fn refuse_recordings_under_another_pipe_wake_latency(
        &self,
        recordings: &[(&str, &Path)],
    ) -> anyhow::Result<()> {
        let trial = RunData::get_raw_runopts(self);
        let config = &trial.det_opts.det_config;
        for &(option, path) in recordings {
            detcore::preemptions::boundary_recording::Recording::read(path)?
                .require_faithful_replay()
                .map_err(|error| {
                    anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
                        "{option} {} cannot supply analysis evidence: {error:#}",
                        path.display()
                    ))
                })?;
            let recorded = detcore::preemptions::read_recorded_pipe_wake_latency(path)
                .map_err(anyhow::Error::msg)?;
            if let Some(refusal) = detcore::preemptions::pipe_wake_latency_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                config.pipe_wake_latency,
            ) {
                return Err(
                    anyhow::Error::new(crate::app::container::PolicyRefusal).context(refusal)
                );
            }
            let recorded = detcore::preemptions::read_recorded_signal_state_copy(path)
                .map_err(anyhow::Error::msg)?;
            if let Some(refusal) = detcore::preemptions::signal_state_copy_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                config.signal_state_copy,
            ) {
                return Err(
                    anyhow::Error::new(crate::app::container::PolicyRefusal).context(refusal)
                );
            }
            let recorded = detcore::preemptions::read_recorded_expired_wait_runs_charged_once(path)
                .map_err(anyhow::Error::msg)?;
            if let Some(refusal) = detcore::preemptions::expired_wait_charge_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                Some(self.trial_expired_wait_charge()),
                true,
            ) {
                return Err(
                    anyhow::Error::new(super::super::container::PolicyRefusal).context(refusal)
                );
            }
            let recorded = detcore::preemptions::read_recorded_cap_preemption_charge(path)
                .map_err(anyhow::Error::msg)?;
            if let Some(refusal) = detcore::preemptions::cap_preemption_charge_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                config.cap_preemption_charge,
            ) {
                return Err(
                    anyhow::Error::new(super::super::container::PolicyRefusal).context(refusal)
                );
            }
            let recorded =
                detcore::preemptions::read_recorded_queued_sleeps_registered_at_first_pass(path)
                    .map_err(anyhow::Error::msg)?;
            if let Some(refusal) = detcore::preemptions::sleep_registration_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                Some(self.trial_sleep_registration()),
                true,
            ) {
                return Err(
                    anyhow::Error::new(crate::app::container::PolicyRefusal).context(refusal)
                );
            }
            let recorded = detcore::preemptions::read_recorded_rewrite_exclusives_in_memory(path)
                .map_err(anyhow::Error::msg)?;
            if let Some(refusal) = detcore::preemptions::rewrite_exclusives_in_memory_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                self.trial_rewrite_exclusives_in_memory(),
            ) {
                return Err(
                    anyhow::Error::new(crate::app::container::PolicyRefusal).context(refusal)
                );
            }
        }
        Ok(())
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
    /// lists: host-network and namespace-less routes, output paths every
    /// trial would share, and a recording every run would replay.
    pub fn refuse_outputs_trials_overwrite(&self) -> anyhow::Result<()> {
        let trial = RunData::get_raw_runopts(self);
        // A recording runs on the host network (`RunOpts::validate_args`), so
        // the trials would stop being reproducible, and every trial would
        // write the same trace path.
        if let Some(path) = trial.record_networking_trace() {
            return Err(
                anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
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
                anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
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
            anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
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
            anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
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

/// The record a trial replays for `events`, a schedule a search built from
/// the schedules of `sources` (the endpoints it searches between, each the
/// record of a run): the source whose schedule shares the longest prefix of
/// events with it, the first on a tie, cut to that prefix
/// ([`PreemptionRecord::schedule_prefix`]) and given `events` as its schedule.
/// The trial then keeps the callback checkpoints the unchanged prefix reaches,
/// with the source's per-thread histories and epoch, so its slices end where
/// the source's did up to the first edit, as a prefix trial's do, all but the
/// slice running at the edit: where that one ends decides the edited event,
/// so the scheduler decides it. An endpoint replayed whole keeps all of its
/// checkpoints, and so does a source whose schedule ends where the trial's
/// goes on (no event of its own follows its last slice). The events alone when no
/// source shares a prefix or its checkpoints do not fit them
/// ([`PreemptionRecord::with_schedule`]). Past its first edit a trial runs on
/// its events alone, as before: the prefix keeps no checkpoint beyond the
/// progress each thread reaches in it.
pub fn trial_record_from(
    events: Vec<SchedEvent>,
    sources: &[&PreemptionRecord],
) -> PreemptionRecord {
    // The longest shared prefix wins; on a tie, a source whose whole
    // schedule is `events` (an endpoint replayed whole, beside another whose
    // schedule only starts with it), then the first.
    let mut best: Option<(usize, bool, &PreemptionRecord)> = None;
    for &source in sources {
        let shared = source
            .schedevents()
            .iter()
            .zip(&events)
            .take_while(|(recorded, trial)| recorded == trial)
            .count();
        let whole = shared == events.len() && shared == source.schedevents().len();
        if shared > 0
            && best.is_none_or(|(longest, was_whole, _)| (shared, whole) > (longest, was_whole))
        {
            best = Some((shared, whole, source));
        }
    }
    best.and_then(|(shared, _, source)| {
        source.schedule_prefix(shared).with_schedule(events.clone())
    })
    .unwrap_or_else(|| PreemptionRecord::from_sched_events(events))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    /// The records analyze edited (minimization's and the baseline search's)
    /// replay with `--replay-edited-preemptions`, and so do the repro
    /// commands it prints for them; a recording replays strictly. Before, a
    /// minimization trial whose knock-out let a thread start a slice past
    /// its recorded end stopped the whole analysis with Detcore's "Cannot
    /// set end of timeslice" panic (exit 101).
    #[test]
    fn analyze_replays_only_its_edited_preemption_records_as_edited() {
        let args =
            crate::app::Args::try_parse_from(["hermit", "analyze", "--", "/bin/true"]).unwrap();
        let crate::app::Subcommand::Analyze(mut options) = args.command else {
            panic!("not analyze")
        };
        options.apply_global(&args.global);
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let record = workspace.path().join("round_001.preempts");
        let edited = RunData::new_baseline(&options, "round_001".to_string())
            .unwrap()
            .with_edited_preempts_path_in(record.clone());
        assert!(edited.runopts.det_opts.det_config.replay_edited_preemptions);
        let repro = edited.to_repro();
        assert!(repro.contains(" --replay-edited-preemptions"), "{repro}");
        // A record analyze builds in memory (the baseline search's
        // knock-outs) is written to the workspace and replayed as edited.
        let knocked_out = RunData::new_baseline(&options, "round_002".to_string())
            .unwrap()
            .with_preempts_in(PreemptionRecord::from_vecs(
                &std::collections::BTreeMap::new(),
            ));
        let config = &knocked_out.runopts.det_opts.det_config;
        assert!(config.replay_edited_preemptions);
        assert!(config.replay_preemptions_from.as_ref().unwrap().is_file());
        let recording = RunData::new_baseline(&options, "verify".to_string())
            .unwrap()
            .with_preempts_path_in(record);
        assert!(
            !recording
                .runopts
                .det_opts
                .det_config
                .replay_edited_preemptions
        );
        assert!(
            !recording.to_repro().contains("--replay-edited-preemptions"),
            "{}",
            recording.to_repro()
        );
    }

    /// The actual writer preserves one format's jumps/charging and refuses
    /// mixed formats before touching its destination, even for an exact prefix.
    #[test]
    fn trial_record_writing_preserves_one_timing_format_and_refuses_mixed_sources() {
        use detcore::preemptions::TimeSkip;
        use detcore::preemptions::TimeSkipReason;
        use detcore::types::DetTid;
        use detcore::types::LogicalTime;

        let events = vec![SchedEvent::branches(DetTid::from_raw(3), 1)];
        let record = |format| {
            let record = PreemptionRecord::from_sched_events(events.clone()).with_format(format);
            if format.is_some() {
                record.with_time_skips(vec![TimeSkip {
                    event: 1,
                    target: LogicalTime::from_nanos(1000),
                    reason: TimeSkipReason::IdleGuestPollers,
                }])
            } else {
                record
            }
        };
        let options = || {
            let args =
                crate::app::Args::try_parse_from(["hermit", "analyze", "--", "/bin/true"]).unwrap();
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
                panic!("not analyze")
            };
            options.apply_global(&args.global);
            options
        };
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("trial.json");
        for format in [None, Some(2), Some(3)] {
            let options = options();
            let source = record(format);
            options.time_skip_sources.lock().unwrap().remember(&source);
            options
                .write_trial_schedule_from(events.clone(), &[], &output)
                .unwrap();
            let written = PreemptionReader::try_new(&output).unwrap().into_inner();
            assert_eq!(written.format_version(), format);
            assert_eq!(written.time_skips(), source.time_skips());
            assert_eq!(written.charges_replayed_switch_ins(), format == Some(3));
        }
        for other in [None, Some(2)] {
            for formats in [[Some(3), other], [other, Some(3)]] {
                let options = options();
                for format in formats {
                    options
                        .time_skip_sources
                        .lock()
                        .unwrap()
                        .remember(&record(format));
                }
                std::fs::write(&output, b"existing output must survive refusal").unwrap();
                let error = options
                    .write_trial_schedule_from(events.clone(), &[], &output)
                    .unwrap_err();
                let message = error.to_string();
                assert!(message.contains("different timing formats"), "{message}");
                assert!(message.contains("record both inputs"), "{message}");
                assert_eq!(
                    std::fs::read(&output).unwrap(),
                    b"existing output must survive refusal"
                );
            }
        }
    }

    /// A trial states the background-starts format only when it keeps a
    /// source's noted starts, a prefix of its whole schedule: a spliced trial
    /// or a shorter prefix of a recording that notes its starts is written in
    /// the format below, which the re-record of that recording's schedule
    /// replay states too, so the two combine as sources.
    #[test]
    fn a_trial_states_the_background_starts_format_only_with_its_starts() {
        use detcore::preemptions::BACKGROUND_STARTS_FORMAT;
        use detcore::preemptions::BackgroundStart;
        use detcore::types::DetTid;

        let tid = DetTid::from_raw(3);
        let events = vec![SchedEvent::branches(tid, 1), SchedEvent::branches(tid, 2)];
        let mut value =
            serde_json::to_value(PreemptionRecord::from_sched_events(events.clone())).unwrap();
        value["format_version"] = serde_json::json!(BACKGROUND_STARTS_FORMAT);
        value["background_starts"] = serde_json::to_value(vec![BackgroundStart {
            after: 1,
            thread: tid,
            call: 1,
        }])
        .unwrap();
        let noting: PreemptionRecord = serde_json::from_value(value).unwrap();
        noting.validate().unwrap();
        assert!(noting.notes_background_starts());
        let re_record = PreemptionRecord::from_sched_events(events.clone())
            .with_format(Some(BACKGROUND_STARTS_FORMAT - 1));
        let args =
            crate::app::Args::try_parse_from(["hermit", "analyze", "--", "/bin/true"]).unwrap();
        let crate::app::Subcommand::Analyze(mut options) = args.command else {
            panic!("not analyze")
        };
        options.apply_global(&args.global);
        options.time_skip_sources.lock().unwrap().remember(&noting);
        options
            .time_skip_sources
            .lock()
            .unwrap()
            .remember(&re_record);
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("trial.json");
        let read = || PreemptionReader::try_new(&output).unwrap().into_inner();

        options
            .write_trial_schedule_from(events.clone(), &[], &output)
            .unwrap();
        let spliced = read();
        assert_eq!(spliced.format_version(), Some(BACKGROUND_STARTS_FORMAT - 1));
        assert!(spliced.background_starts().is_empty());

        options
            .write_trial_record(noting.schedule_prefix(1), &output)
            .unwrap();
        let shorter = read();
        assert_eq!(shorter.format_version(), Some(BACKGROUND_STARTS_FORMAT - 1));
        assert!(shorter.background_starts().is_empty());

        options
            .write_trial_record(noting.schedule_prefix(events.len()), &output)
            .unwrap();
        let whole = read();
        assert_eq!(whole.format_version(), Some(BACKGROUND_STARTS_FORMAT));
        assert_eq!(whole.background_starts(), noting.background_starts());

        // A whole recorded schedule that notes its starts keeps its own jumps
        // beside them: two recordings of the same events, S in format 3 and T
        // in format 4 with its starts and other jumps, never lend each other
        // their jumps in a whole-schedule trial, in either order. S, which
        // notes no starts, takes one recording's jumps whole.
        let jump = |target| detcore::preemptions::TimeSkip {
            event: 1,
            target: detcore::types::LogicalTime::from_nanos(target),
            reason: detcore::preemptions::TimeSkipReason::IdleGuestPollers,
        };
        let s = PreemptionRecord::from_sched_events(events.clone())
            .with_format(Some(BACKGROUND_STARTS_FORMAT - 1))
            .with_time_skips(vec![jump(1000)]);
        let mut value = serde_json::to_value(&noting).unwrap();
        value["time_skips"] = serde_json::to_value(vec![jump(2000)]).unwrap();
        let t: PreemptionRecord = serde_json::from_value(value).unwrap();
        t.validate().unwrap();
        assert!(t.notes_background_starts());
        for order in [[&s, &t], [&t, &s]] {
            *options.time_skip_sources.lock().unwrap() = Default::default();
            for source in order {
                options.time_skip_sources.lock().unwrap().remember(source);
            }
            let whole = |source: &PreemptionRecord| {
                options
                    .write_trial_record(source.schedule_prefix(events.len()), &output)
                    .unwrap();
                read()
            };
            let written = whole(&t);
            assert_eq!(written.format_version(), Some(BACKGROUND_STARTS_FORMAT));
            assert_eq!(written.time_skips(), Some(&[jump(2000)][..]));
            assert_eq!(written.background_starts(), t.background_starts());
            let written = whole(&s);
            assert_eq!(written.format_version(), Some(BACKGROUND_STARTS_FORMAT - 1));
            assert!(written.background_starts().is_empty());
            assert_eq!(written.time_skips(), order[0].time_skips());
        }

        // A source of a format this Hermit does not read keeps it as a source,
        // so a trial from it states it too and is refused by name, not
        // lowered into a format this Hermit reads.
        let newer = detcore::preemptions::PREEMPTION_RECORD_FORMAT + 1;
        *options.time_skip_sources.lock().unwrap() = Default::default();
        options.time_skip_sources.lock().unwrap().remember(
            &PreemptionRecord::from_sched_events(events.clone()).with_format(Some(newer)),
        );
        let refusal = options
            .write_trial_schedule_from(events.clone(), &[], &output)
            .unwrap_err();
        assert!(
            format!("{refusal:#}").contains(&format!("record format Some({newer})")),
            "{refusal:#}"
        );
    }

    /// Analyze and bisect run every trial under the timeslice a recording says
    /// its run drew (`--chaos-timeslice-draw=run`): an explicit
    /// `--max-timeslice` passed to the trials, an equal explicit one kept, a
    /// different one refused. Run arguments that draw per run are refused,
    /// and a per-slice range beside a drawn recording is refused too.
    #[test]
    fn trials_run_under_the_timeslice_a_recording_drew() {
        let options = |run_args: &[&str]| {
            let mut argv: Vec<String> = vec!["hermit".into(), "analyze".into()];
            argv.extend(run_args.iter().map(|arg| format!("--run-arg={arg}")));
            argv.extend(["--".into(), "/bin/true".into()]);
            let args = crate::app::Args::try_parse_from(&argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::app::Subcommand::Analyze(options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            *options
        };
        let dir = tempfile::tempdir().unwrap();
        let drawn = dir.path().join("drawn.json");
        detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
            .with_chaos_timeslice(Some(4321))
            .write_to_disk(&drawn)
            .unwrap();
        let other = dir.path().join("other.json");
        detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
            .with_chaos_timeslice(Some(9999))
            .write_to_disk(&other)
            .unwrap();
        let plain = dir.path().join("plain.json");
        detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
            .write_to_disk(&plain)
            .unwrap();
        let recordings = [("--run1-preemptions", drawn.as_path())];

        let mut adopting = options(&[]);
        adopting.adopt_recorded_timeslice(&recordings).unwrap();
        assert!(
            adopting
                .run_arg
                .iter()
                .any(|arg| arg == "--max-timeslice=4321"),
            "{:?}",
            adopting.run_arg
        );
        let mut equal = options(&["--max-timeslice=4321"]);
        equal.adopt_recorded_timeslice(&recordings).unwrap();
        assert_eq!(
            equal.run_arg,
            ["--max-timeslice=4321"],
            "added a second one"
        );
        let mut unmarked = options(&[]);
        unmarked
            .adopt_recorded_timeslice(&[("--run1-preemptions", plain.as_path())])
            .unwrap();
        assert!(
            unmarked
                .run_arg
                .iter()
                .all(|arg| !arg.starts_with("--max-timeslice"))
        );
        // A per-slice range alone is fine without a drawn recording.
        let mut slices = options(&["--chaos", "--chaos-timeslice-range=1000..2000"]);
        slices
            .adopt_recorded_timeslice(&[("--run1-preemptions", plain.as_path())])
            .unwrap();

        for (run_args, recordings, expected) in [
            (
                &["--chaos", "--chaos-timeslice-draw=run"][..],
                &[][..],
                "every trial must run under one",
            ),
            (
                &["--max-timeslice=5000"][..],
                &recordings[..],
                "pass --max-timeslice=4321",
            ),
            (
                &[],
                &[
                    ("--run1-preemptions", drawn.as_path()),
                    ("--run2-preemptions", other.as_path()),
                ][..],
                "Record both under the same",
            ),
            (
                &[
                    "--chaos",
                    "--chaos-timeslice-range=1000..2000",
                    "--chaos-timeslice-draw=run",
                ],
                &[],
                "every trial must run under one",
            ),
            (
                &["--chaos", "--chaos-timeslice-range=1000..2000"],
                &recordings[..],
                "Remove --chaos-timeslice-range",
            ),
        ] {
            let error = options(run_args)
                .adopt_recorded_timeslice(recordings)
                .unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
                    .is_some(),
                "{error:#}"
            );
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
    }

    /// The target run of `--run1-preemptions` replays a copy of the record in
    /// the workspace (the guest's container does not see the host's /tmp),
    /// and an explicit `--trial-timeout` bounds it.
    #[test]
    fn a_recorded_target_run_replays_a_workspace_copy_within_an_explicit_bound() {
        let elsewhere = tempfile::tempdir().unwrap();
        let record = elsewhere.path().join("target.preempts");
        std::fs::write(&record, "{}").unwrap();
        let run1_preemptions = format!("--run1-preemptions={}", record.display());
        let args = crate::app::Args::try_parse_from([
            "hermit",
            "analyze",
            "--trial-timeout=7",
            run1_preemptions.as_str(),
            "--",
            "/bin/true",
        ])
        .unwrap();
        let crate::app::Subcommand::Analyze(mut options) = args.command else {
            panic!("not analyze")
        };
        options.apply_global(&args.global);
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_run1_target(&options, "run1_target".to_string()).unwrap();
        let replayed = run
            .runopts
            .det_opts
            .det_config
            .replay_preemptions_from
            .clone();
        let copy = workspace.path().join("run1_target.preempts");
        assert_eq!(replayed.as_deref(), Some(copy.as_path()));
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "{}");
        assert_eq!(run.runopts.run_timeout(), Some(Duration::from_secs(7)));
    }

    /// Round-2 review of https://github.com/rrnewton/hermit/pull/3686, finding
    /// 2: the refusal `RunOpts::main` makes never sees an analyze or bisect
    /// trial, which goes straight to `RunOpts::run`. Those commands admit
    /// ptrace and KVM, and `--no-namespace`, passed through the run arguments,
    /// is refused with either (round-4c review, finding 1).
    #[test]
    fn log_cap_refusal_reaches_analyze_and_bisect_trials() {
        let options = |argv: &[&str]| {
            let args = crate::app::Args::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
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
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
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
            let args = crate::app::Args::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
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
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
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
                    "--backend=liteinst",
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
        // KVM trials read the branch counter since Reverie 9443a6b2, so a
        // strict KVM trial on an inexact counter refuses, naming its backend.
        let error = options(
            &[
                "hermit",
                "--backend=kvm",
                "analyze",
                "--",
                "--strict",
                "/bin/true",
            ],
            inexact,
        )
        .refuse_strict_with_inexact_branch_counter()
        .expect_err("a strict KVM trial on an inexact counter");
        assert!(
            error
                .downcast_ref::<crate::app::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        assert!(error.to_string().contains("`kvm` backend"), "{error:#}");
    }

    /// Without `--strict`, analyze and bisect trials on a counter that fails
    /// Reverie's validation go on, and `admit_branch_counter` writes one
    /// warning line for all of them and keeps it, without its newline, for the
    /// final report (`branch_counter_warning`). With `--strict` they are
    /// refused and nothing is written. A counter that passed, a backend that
    /// does not read it, or trials without a timeslice write nothing: the
    /// warning goes exactly where the `--strict` refusal would.
    #[test]
    fn trials_without_strict_warn_once_of_an_inexact_branch_counter() {
        let options = |argv: &[&str], probe: fn() -> Option<String>| {
            let args = crate::app::Args::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options.inexact_branch_counter = probe;
            options
        };
        let admit = |options: &mut AnalyzeOpts| {
            let mut lines = Vec::new();
            let result =
                options.admit_branch_counter(&mut |line: &str| lines.push(line.to_owned()));
            (result, lines)
        };
        let inexact: fn() -> Option<String> = || Some("SpecLockMap is enabled".to_string());
        let warning = crate::app::run::warning_for_an_inexact_branch_counter(
            crate::app::run::CounterReader::Trials,
            inexact,
        )
        .unwrap();
        for warned in [
            &["hermit", "analyze", "--", "/bin/true"][..],
            &["hermit", "--backend=ptrace", "analyze", "--", "/bin/true"][..],
            // KVM reads the counter since Reverie 9443a6b2.
            &["hermit", "--backend=kvm", "analyze", "--", "/bin/true"][..],
        ] {
            let mut options = options(warned, inexact);
            let (result, lines) = admit(&mut options);
            assert!(result.is_ok(), "{warned:?}: {result:?}");
            assert_eq!(lines, std::slice::from_ref(&warning), "{warned:?}");
            assert_eq!(
                options.branch_counter_warning.as_deref(),
                Some(warning.trim_end()),
                "{warned:?}"
            );
        }

        let mut strict = options(
            &["hermit", "analyze", "--", "--strict", "/bin/true"],
            inexact,
        );
        let (result, lines) = admit(&mut strict);
        assert!(
            result.is_err_and(|error| {
                error
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
                    .is_some()
            }),
            "--strict refuses"
        );
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(strict.branch_counter_warning, None);

        let unreachable: fn() -> Option<String> = || panic!("the counter is not consulted");
        for (silent, probe) in [
            (
                &["hermit", "analyze", "--", "/bin/true"][..],
                (|| None) as fn() -> _,
            ),
            (
                &["hermit", "--backend=liteinst", "analyze", "--", "/bin/true"][..],
                unreachable,
            ),
            (
                &[
                    "hermit",
                    "analyze",
                    "--",
                    "--max-timeslice=disabled",
                    "/bin/true",
                ][..],
                unreachable,
            ),
        ] {
            let mut options = options(silent, probe);
            let (result, lines) = admit(&mut options);
            assert!(
                result.is_ok() && lines.is_empty(),
                "{silent:?}: {result:?} {lines:?}"
            );
            assert_eq!(options.branch_counter_warning, None, "{silent:?}");
        }
    }

    /// Trials start through `RunOpts::run`, so run options that only
    /// `RunOpts::main` applies are refused, each named, instead of being
    /// dropped from every trial while the printed reproducer keeps them.
    /// Options the trials do apply are admitted.
    #[test]
    fn trials_refuse_run_options_only_main_applies() {
        let options = |argv: &[&str]| {
            let args = crate::app::Args::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
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
            (
                &["--cpu-profile", "x86-64-v3"][..],
                &["--cpu-profile x86-64-v3"][..],
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
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
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
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
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
                .downcast_ref::<crate::app::container::PolicyRefusal>()
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
        // Host-network and namespace-less routes, output paths every trial
        // would share, and a recording every run would replay
        // (`RunOpts::options_trials_refuse`).
        for (run_arg, named) in [
            ("--strace-only", "--strace-only"),
            ("--no-sequentialize-threads", "--no-sequentialize-threads"),
            ("--network=host", "--network=host"),
            ("--no-namespace", "--no-namespace"),
            ("--gdbserver", "--gdbserver"),
            ("--dap-listen=127.0.0.1:0", "--dap-listen"),
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
            (
                "--record-access-trace-to=requested.access",
                "--record-access-trace-to",
            ),
            (
                "--replay-schedule-from=requested.preempts",
                "--replay-schedule-from",
            ),
            (
                "--replay-preemptions-from=requested.preempts",
                "--replay-preemptions-from",
            ),
        ] {
            // Where this host may choose IDs, --no-namespace is admitted and
            // its trials take chosen IDs (`RunOpts::prepare_trial_guest_ids`).
            if run_arg == "--no-namespace" && crate::app::guest_ids::may_choose_ids().is_ok() {
                options(&[run_arg])
                    .refuse_outputs_trials_overwrite()
                    .unwrap_or_else(|error| panic!("{run_arg}: {error:#}"));
                continue;
            }
            let error = options(&[run_arg])
                .refuse_outputs_trials_overwrite()
                .expect_err("an option the trials cannot take");
            assert!(
                error
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
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
            let args = crate::app::Args::try_parse_from(argv).unwrap();
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
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
                .downcast_ref::<crate::app::container::PolicyRefusal>()
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
                "--dap-listen=127.0.0.1:0",
                "refuse --dap-listen in the run arguments",
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
            // Where this host may choose IDs, --no-namespace is admitted and
            // its trials take chosen IDs (`RunOpts::prepare_trial_guest_ids`).
            if extra == "--no-namespace" && crate::app::guest_ids::may_choose_ids().is_ok() {
                continue;
            }
            let mut argv = vec!["hermit"];
            argv.extend(backend);
            argv.extend([
                "analyze",
                "--run1-schedule=/nonexistent/schedule.json",
                "--",
                extra,
                "/bin/true",
            ]);
            let args = crate::app::Args::try_parse_from(&argv).unwrap();
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            let error = options.main(&args.global).unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
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
    /// right after that preflight, which stops it before any workspace. That
    /// configuration is per-process, so the test runs in a process of its own.
    #[test]
    fn analyze_installs_the_trials_skid_margin() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        let argv = [
            "hermit",
            "analyze",
            "--run1-schedule=/nonexistent/schedule.json",
            "--",
            "--skid-margin=4321",
            "/bin/true",
        ];
        let args = crate::app::Args::try_parse_from(argv).unwrap();
        let crate::app::Subcommand::Analyze(mut options) = args.command else {
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
    /// A reproducer's directory is quoted for the shell: one with spaces and
    /// an apostrophe splits back to itself, and `sh` lands in it.
    #[test]
    fn a_reproducers_directory_is_quoted_for_the_shell() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("it's a dir");
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.to_str().unwrap();
        let line = reproducer_line(Some(dir), "pwd -P".to_owned());
        let words = shell_words::split(&line).unwrap();
        assert_eq!(words[..3], ["cd", dir, "&&"], "{line}");
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&line)
            .current_dir(parent.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            Path::new(String::from_utf8(output.stdout).unwrap().trim()),
            Path::new(dir).canonicalize().unwrap()
        );
        assert_eq!(reproducer_line(None, "hermit run".to_owned()), "hermit run");
    }

    /// A reproducer changes to the directory its trial ran in before it runs
    /// the command, so the guest starts in the trial's working directory
    /// wherever the line is run (#4812). It does so with `--workdir` too,
    /// which leaves Hermit's own relative paths depending on its directory.
    #[test]
    fn analyzer_reproducer_starts_in_the_trials_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&options, "cwd-repro".to_owned()).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let line = run.to_repro();
        let words = shell_words::split(&line).unwrap();
        assert_eq!(words[..3], ["cd", cwd.to_str().unwrap(), "&&"], "{line}");
        assert_eq!(
            words[3..],
            shell_words::split(&run.repro_command()).unwrap()[..],
            "{line}"
        );

        // Run from another directory, the line's own `cd` lands where the
        // trial ran.
        let elsewhere = tempfile::tempdir().unwrap();
        let (cd, _) = line.split_once(" && ").unwrap();
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{cd} && pwd -P"))
            .current_dir(elsewhere.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            Path::new(String::from_utf8(output.stdout).unwrap().trim()),
            cwd.canonicalize().unwrap()
        );

        let mut pinned =
            AnalyzeOpts::try_parse_from(["analyze", "--", "--workdir=/", "/bin/true"]).unwrap();
        pinned.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&pinned, "workdir-repro".to_owned()).unwrap();
        let line = run.to_repro();
        let words = shell_words::split(&line).unwrap();
        assert_eq!(words[..3], ["cd", cwd.to_str().unwrap(), "&&"], "{line}");
        assert!(words.contains(&"--workdir=/".to_owned()), "{line}");
    }

    #[test]
    fn analyzer_reproducer_retains_the_trial_timeout() {
        use clap::CommandFactory;
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", "--", "--timeout=7", "/bin/true"]).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&options, "timeout-repro".to_owned()).unwrap();
        let repro = run.repro_command();
        assert_eq!(repro.matches(" --timeout=7").count(), 1, "{repro}");
        let matches = crate::app::Args::command()
            .try_get_matches_from(shell_words::split(&repro).unwrap())
            .unwrap();
        let args = crate::app::args_from_matches_with_clock(&matches, || {
            panic!("analyzer reproduction must retain its actual fixed input")
        })
        .unwrap();
        let crate::app::Subcommand::Run(mut parsed) = args.command else {
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
        let args = crate::app::Args::try_parse_from(argv).unwrap();
        let crate::app::Subcommand::Analyze(mut options) = args.command else {
            panic!("{argv:?} is not analyze")
        };
        options.inexact_branch_counter = || Some("SpecLockMap is enabled".to_string());
        let error = options.main(&args.global).unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::app::container::PolicyRefusal>()
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
    /// invocation's shared budget. A trial that writes no log file traces
    /// to hermit's stderr, so an exempt trial would put uncapped output on the
    /// public stream (review of https://github.com/rrnewton/hermit/pull/3686,
    /// round 1).
    #[test]
    fn analyze_trials_charge_the_invocations_log_budget() {
        let argv = ["hermit", "--max-log-bytes=4K", "analyze", "--", "/bin/true"];
        let mut args = crate::app::Args::try_parse_from(argv).unwrap();
        args.global.prepare_log_budget().unwrap();
        let crate::app::Subcommand::Analyze(mut options) = args.command else {
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

    /// Parse `analyze OPTIONS -- /bin/true` with a workspace for its runs.
    fn options_with_workspace(options: &[&str]) -> (AnalyzeOpts, tempfile::TempDir) {
        let mut argv = vec!["analyze"];
        argv.extend_from_slice(options);
        argv.extend(["--", "/bin/true"]);
        let mut parsed = AnalyzeOpts::try_parse_from(&argv).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        parsed.tmp_dir = Some(workspace.path().to_path_buf());
        (parsed, workspace)
    }

    /// `--verbose` used to run every trial at debug into a log file of its
    /// own: a gigabyte per trial on a program with ~760k scheduling events,
    /// and a trial several times slower. It now prints analyze's own detail
    /// only: a trial logs at the default warn level to stderr, and neither
    /// its options nor its reproducer carry a debug log.
    #[test]
    fn a_verbose_trial_keeps_the_default_log_level() {
        let (options, workspace) = options_with_workspace(&["-v"]);
        let run = RunData::new_baseline(&options, "trial".to_owned()).unwrap();
        let trial = run.trial_global_opts();
        assert_eq!(trial.log, Some(LevelFilter::WARN));
        assert_eq!(trial.log_file, None);
        let repro = run.repro_command();
        assert!(repro.starts_with("hermit run "), "{repro}");
        assert!(!repro.contains("--log"), "{repro}");
        drop(workspace);
    }

    /// `--trial-log=debug` restores the debug log file: the trial's options
    /// and its reproducer name the level and the file in the workspace.
    #[test]
    fn trial_log_debug_writes_each_trials_debug_log() {
        let (options, workspace) = options_with_workspace(&["-v", "--trial-log=debug"]);
        let run = RunData::new_baseline(&options, "trial".to_owned()).unwrap();
        let trial = run.trial_global_opts();
        let log = workspace.path().join("trial.log");
        assert_eq!(trial.log, Some(LevelFilter::DEBUG));
        assert_eq!(trial.log_file.as_deref(), Some(log.as_path()));
        let repro = run.repro_command();
        let expected = format!("hermit --log=debug --log-file={} run ", log.display());
        assert!(repro.starts_with(&expected), "{repro}");
    }

    /// Each `--trial-log` level, and `--guest-log`'s, which `--trial-log`
    /// overrides: a file above warn, stderr at warn and below.
    #[test]
    fn trial_log_sets_the_level_and_the_file() {
        for (flags, level, file, repro) in [
            (&[][..], LevelFilter::WARN, false, ""),
            (
                &["--trial-log=off"][..],
                LevelFilter::ERROR,
                false,
                " --log=error",
            ),
            (&["--trial-log=warn"][..], LevelFilter::WARN, false, ""),
            (
                &["--trial-log=info"][..],
                LevelFilter::INFO,
                true,
                " --log=info",
            ),
            (
                &["-g", "trace"][..],
                LevelFilter::TRACE,
                true,
                " --log=trace",
            ),
            (
                &["-g", "error"][..],
                LevelFilter::ERROR,
                false,
                " --log=error",
            ),
            (
                &["-g", "debug", "--trial-log=warn"][..],
                LevelFilter::WARN,
                false,
                "",
            ),
        ] {
            let (options, workspace) = options_with_workspace(flags);
            let run = RunData::new_baseline(&options, "trial".to_owned()).unwrap();
            let trial = run.trial_global_opts();
            assert_eq!(trial.log, Some(level), "{flags:?}");
            let log = workspace.path().join("trial.log");
            assert_eq!(trial.log_file, file.then(|| log.clone()), "{flags:?}");
            let expected = if file {
                format!("hermit{repro} --log-file={} run ", log.display())
            } else {
                format!("hermit{repro} run ")
            };
            assert!(
                run.repro_command().starts_with(&expected),
                "{flags:?}: {}",
                run.repro_command()
            );
        }
        assert!(
            AnalyzeOpts::try_parse_from(["analyze", "--trial-log=loud", "--", "/bin/true"])
                .is_err()
        );
    }

    /// `--selfcheck` compares the target run's debug log with its replay's,
    /// so every run whose log it may compare logs at debug or above into its
    /// file, whatever `--trial-log` says: the target run, its replay, a
    /// `--search` run (the one that matches becomes the target), a
    /// `--minimize` round (the last one to match is the minimized target)
    /// and the minimized target itself. Each is built as analyze builds it.
    /// The other trials keep the `--trial-log` level, and without
    /// `--selfcheck` the compared runs do too.
    #[test]
    fn selfcheck_logs_the_runs_it_compares_at_debug() {
        for (flags, compared_level, other_level, other_file) in [
            (
                &["--selfcheck"][..],
                Some(LevelFilter::DEBUG),
                LevelFilter::WARN,
                false,
            ),
            (
                &["--selfcheck", "--trial-log=off"][..],
                Some(LevelFilter::DEBUG),
                LevelFilter::ERROR,
                false,
            ),
            (
                &["--selfcheck", "-g", "trace"][..],
                Some(LevelFilter::TRACE),
                LevelFilter::TRACE,
                true,
            ),
            (&[][..], None, LevelFilter::WARN, false),
        ] {
            let (options, workspace) = options_with_workspace(flags);
            let preempts = workspace.path().join("record.preempts");
            let minimized_log = workspace.path().join("round_007.log");
            let target = RunData::new_run1_target(&options, "target".to_owned()).unwrap();
            let replay =
                RunData::new_run1_replay(&options, "replay".to_owned(), &target.runopts).unwrap();
            let search = options.search_run(3, 17).unwrap();
            let round = options.preempts_replay_run("round_001", &preempts).unwrap();
            let minimized = RunData::from_minimize_output(
                &options,
                "minimized".to_owned(),
                target.runopts.clone(),
                PreemptionRecord::default(),
                preempts.clone(),
                minimized_log.clone(),
            );
            for run in [&target, &replay, &search, &round, &minimized] {
                let trial = run.trial_global_opts();
                let log = if run.name() == "minimized" {
                    minimized_log.clone()
                } else {
                    workspace.path().join(format!("{}.log", run.name()))
                };
                match compared_level {
                    Some(level) => {
                        assert_eq!(trial.log, Some(level), "{flags:?} {}", run.name());
                        assert_eq!(
                            trial.log_file,
                            Some(log.clone()),
                            "{flags:?} {}",
                            run.name()
                        );
                        let logging = format!("hermit --log={level} --log-file={} ", log.display());
                        assert!(
                            run.repro_command().starts_with(&logging),
                            "{flags:?} {}",
                            run.repro_command()
                        );
                    }
                    None => {
                        assert_eq!(trial.log, Some(other_level), "{flags:?} {}", run.name());
                        assert_eq!(trial.log_file, None, "{flags:?} {}", run.name());
                    }
                }
            }
            let other = RunData::new_baseline(&options, "prefix_round_001".to_owned()).unwrap();
            let trial = other.trial_global_opts();
            assert_eq!(trial.log, Some(other_level), "{flags:?}");
            assert_eq!(
                trial.log_file.is_some(),
                other_file,
                "{flags:?}: {:?}",
                trial.log_file
            );
        }
    }

    /// A refused trial's warning says its reason is in hermit's log above, or
    /// in the trial's log file: Detcore logs that reason as an error, so every
    /// level keeps errors, `--trial-log=off` and `-g off` included.
    #[test]
    fn every_trial_log_level_keeps_errors() {
        for flags in [
            &["--trial-log=off"][..],
            &["-g", "off"][..],
            &["-g", "error"][..],
        ] {
            let (options, _workspace) = options_with_workspace(flags);
            assert_eq!(options.trial_log_level(), LevelFilter::ERROR, "{flags:?}");
            let run = RunData::new_baseline(&options, "trial".to_owned()).unwrap();
            assert_eq!(
                run.trial_global_opts().log,
                Some(LevelFilter::ERROR),
                "{flags:?}"
            );
        }
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
            let args = crate::app::Args::try_parse_from(&argv).unwrap();
            args.command
                .validate_backend_scope(args.global.backend)
                .unwrap_or_else(|error| panic!("{argv:?} must be in scope: {error:#}"));
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
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
            let repro = run.repro_command();
            assert!(
                repro.contains(&format!(" --backend={flag} run ")),
                "{flag}: reproducer does not select the backend: {repro}"
            );
        }
    }

    /// A trial keeps the maximum its run arguments request: the inherited one, a
    /// disabled one, or an explicit one where the backend delivers timer events
    /// (KVM, as ptrace); a backend without the timer (in-guest LiteInst)
    /// refuses the explicit request by name.
    #[test]
    fn trial_preemption_keeps_the_explicit_maximum_request() {
        for (backend, delivers_timer) in [("kvm", true), ("liteinst", false)] {
            for maximum in [
                None,
                Some("--max-timeslice=disabled"),
                Some("--max-timeslice=200000000"),
            ] {
                let backend_flag = format!("--backend={backend}");
                let mut argv = vec!["hermit", backend_flag.as_str(), "analyze", "--"];
                argv.extend(maximum);
                argv.push("/bin/true");
                let args = crate::app::Args::try_parse_from(argv).unwrap();
                let crate::app::Subcommand::Analyze(mut options) = args.command else {
                    panic!("expected analyze")
                };
                options.apply_global(&args.global);
                let workspace = tempfile::tempdir().unwrap();
                options.tmp_dir = Some(workspace.path().to_path_buf());
                let result = RunData::new_baseline(&options, "preemption".to_owned());
                if maximum == Some("--max-timeslice=200000000") && !delivers_timer {
                    let error = result.err().expect("explicit maximum must refuse");
                    assert!(
                        error
                            .downcast_ref::<crate::app::container::PolicyRefusal>()
                            .is_some()
                    );
                    assert!(format!("{error:#}").contains(&format!(
                        "unsupported_preemption: --backend={backend} refuses --max-timeslice"
                    )));
                } else if cfg!(feature = "liteinst") && !delivers_timer && maximum.is_none() {
                    // The inherited maximum passes the timer check; a build
                    // with the in-guest runtime then refuses any maximum on
                    // in-guest LiteInst in its later in-guest check.
                    let error = result.err().expect("in-guest LiteInst refuses a maximum");
                    assert!(
                        error
                            .downcast_ref::<crate::app::container::PolicyRefusal>()
                            .is_some(),
                        "{error:#}"
                    );
                    let message = format!("{error:#}");
                    assert!(
                        message.contains("pass --max-timeslice=disabled"),
                        "{message}"
                    );
                    assert!(!message.contains("unsupported_preemption"), "{message}");
                } else {
                    assert_eq!(
                        result
                            .unwrap_or_else(|error| panic!("{backend} {maximum:?}: {error:#}"))
                            .runopts
                            .det_opts
                            .det_config
                            .max_timeslice
                            .map(u64::from),
                        if maximum == Some("--max-timeslice=disabled") {
                            None
                        } else {
                            Some(200_000_000)
                        },
                        "{backend} {maximum:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn trials_bind_only_a_workspace_the_private_tmp_would_hide() {
        let mut options = AnalyzeOpts::try_parse_from([
            "analyze",
            "--run1-seed=6",
            "--",
            "--epoch=2026-01-01T00:00:00Z",
            "/bin/true",
        ])
        .unwrap();
        let binds = |options: &AnalyzeOpts| -> Vec<String> {
            RunData::new_baseline(options, "trial".to_owned())
                .unwrap()
                .runopts
                .bind
                .iter()
                .map(|bind| bind.target.to_string_lossy().into_owned())
                .collect()
        };

        options.tmp_dir = Some(PathBuf::from("/tmp/hermit_analyzeXYZ"));
        assert_eq!(binds(&options), ["/tmp/hermit_analyzeXYZ"]);

        options.tmp_dir = Some(PathBuf::from("someone/analyze-ws/hermit_analyzeXYZ"));
        assert!(binds(&options).is_empty(), "visible as it is");

        // Where the workspace really is decides. A relative path to a
        // directory under /tmp is bound, as its canonical path.
        let under_tmp = tempfile::Builder::new()
            .prefix("hermit_analyze_bind")
            .tempdir_in("/tmp")
            .unwrap();
        let canonical = std::fs::canonicalize(under_tmp.path()).unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert!(canonical.starts_with("/tmp"), "{}", canonical.display());
        let relative = PathBuf::from("../".repeat(cwd.components().count() - 1))
            .join(canonical.strip_prefix("/").unwrap());
        assert!(relative.is_relative());
        options.tmp_dir = Some(relative);
        assert_eq!(binds(&options), [canonical.to_str().unwrap()]);
        // A symlink under /tmp that leads out of it is not: the workspace is
        // visible where it really is.
        let link = canonical.join("out-of-tmp");
        std::os::unix::fs::symlink("/", &link).unwrap();
        options.tmp_dir = Some(link);
        assert!(binds(&options).is_empty(), "{:?}", binds(&options));
    }

    #[test]
    fn every_trial_hands_the_guest_the_target_seed_inputs() {
        let mut options = AnalyzeOpts::try_parse_from([
            "analyze",
            "--run1-seed=6",
            "--run2-seed=1",
            "--",
            "--chaos",
            "--epoch=2026-01-01T00:00:00Z",
            "/bin/true",
        ])
        .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());

        let target = RunData::new_run1_target(&options, "target".to_owned()).unwrap();
        let target = &target.runopts.det_opts.det_config;
        assert_eq!(
            (target.sched_seed(), target.rng_seed()),
            (6, 6),
            "the failing run as found"
        );

        // A baseline or a schedule replay changes the schedule seed only.
        let mut baseline = RunData::new_baseline(&options, "baseline".to_owned()).unwrap();
        baseline.runopts.det_opts.det_config.seed = 1;
        let baseline = &baseline.runopts.det_opts.det_config;
        assert_eq!(baseline.sched_seed(), 1);
        assert_eq!((baseline.rng_seed(), baseline.fuzz_seed()), (6, 6));

        let mut explicit = options.clone();
        explicit.run_args = [
            "--chaos",
            "--rng-seed=9",
            "--epoch=2026-01-01T00:00:00Z",
            "/bin/true",
        ]
        .map(String::from)
        .to_vec();
        let replay = RunData::new_baseline(&explicit, "replay".to_owned()).unwrap();
        assert_eq!(
            replay.runopts.det_opts.det_config.rng_seed(),
            9,
            "an explicit --rng-seed in the run arguments is kept"
        );
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
            let repro = run.repro_command();
            assert_eq!(repro.matches("--epoch=").count(), 1, "{repro}");
            let matches = crate::app::Args::command()
                .try_get_matches_from(shell_words::split(&repro).unwrap())
                .unwrap();
            let args = crate::app::args_from_matches_with_clock(&matches, || {
                panic!("analyzer reproduction must retain its actual fixed input")
            })
            .unwrap();
            let crate::app::Subcommand::Run(parsed) = args.command else {
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

    #[test]
    fn a_refused_run_keeps_its_type_reason_and_log_location() {
        use crate::app::container::LogCapExceeded;
        use crate::app::container::PolicyRefusal;
        use crate::app::container::RunTimeoutMarker;

        for (verbose, selfcheck) in [(false, false), (true, false), (false, true)] {
            let dir = tempfile::tempdir().unwrap();
            let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
            options.tmp_dir = Some(dir.path().to_path_buf());
            options.verbose = verbose;
            options.selfcheck = selfcheck;
            let run = RunData::new_baseline(&options, "refused_trial".to_owned()).unwrap();
            let mut global = run.trial_global_opts();
            global.open_log_file().unwrap();
            for error in [
                anyhow::Error::new(PolicyRefusal),
                anyhow::Error::new(PolicyRefusal).context("unsupported operation: restart_syscall"),
                anyhow::Error::msg("unsupported operation: restart_syscall").context(PolicyRefusal),
                anyhow::Error::new(PolicyRefusal).context("unsupported configuration at admission"),
            ] {
                let reason = format!("{error:#}");
                let error = run.run_error(error, global.log_file.as_deref());
                assert!(error.downcast_ref::<PolicyRefusal>().is_some());
                let message = format!("{error:#}");
                assert!(message.contains(&reason), "{message}");
                assert!(message.contains("Hermit refused analysis run refused_trial"));
                assert!(message.contains("no program outcome was established"));
                assert!(
                    message.contains("use a supported operation or configuration before retrying")
                );
                if let Some(path) = &global.log_file {
                    assert!(path.exists());
                    assert!(message.contains(&format!("the trial log {}", path.display())));
                } else {
                    assert!(message.contains("Hermit's stderr above"));
                }
                assert!(!run.has_launched());
            }
            for error in [
                anyhow::Error::new(RunTimeoutMarker),
                anyhow::Error::new(LogCapExceeded),
            ] {
                let before = format!("{error:#}");
                let error = run.run_error(error, global.log_file.as_deref());
                assert_eq!(format!("{error:#}"), before);
                assert!(
                    error.downcast_ref::<RunTimeoutMarker>().is_some()
                        || error.downcast_ref::<LogCapExceeded>().is_some()
                );
            }
        }
    }

    /// A trial writes the schedule or the preemption record it replays into
    /// the workspace, never into a file the run arguments name
    /// (`--replay-schedule-from`, `--replay-preemptions-from`): that file is
    /// the user's own and stays as it was.
    #[test]
    fn a_trial_writes_what_it_replays_only_into_the_workspace() {
        for option in ["--replay-schedule-from", "--replay-preemptions-from"] {
            let outside = tempfile::tempdir().unwrap();
            let users = outside.path().join("users.preempts");
            // Content no trial writes, so that a write into it shows.
            PreemptionRecord::from_sched_events(Vec::new())
                .with_pipe_wake_latency(Some(1))
                .write_to_disk(&users)
                .unwrap();
            let before = std::fs::read(&users).unwrap();
            let replay = format!("{option}={}", users.display());
            let mut options =
                AnalyzeOpts::try_parse_from(["analyze", "--", replay.as_str(), "/bin/true"])
                    .unwrap();
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            // As the bisection builds and writes a trial (`event_level_search`).
            let mut trial = RunData::new_baseline(&options, "bisect_round_001".to_owned())
                .unwrap()
                .with_schedule_replay()
                .with_schedule_recording();
            let schedule = trial.sched_path_in().to_path_buf();
            assert_eq!(schedule, workspace.path().join("bisect_round_001.preempts"));
            assert_eq!(
                trial.runopts.det_opts.det_config.replay_schedule_from,
                Some(schedule.clone())
            );
            options
                .write_trial_schedule_from(Vec::new(), &[], &schedule)
                .unwrap();
            // As the check of the critical preemption builds its run.
            let check = RunData::new_baseline(&options, "verify".to_owned())
                .unwrap()
                .with_preempts_in(PreemptionRecord::from_sched_events(Vec::new()));
            assert_eq!(
                check.runopts.det_opts.det_config.replay_preemptions_from,
                Some(workspace.path().join("verify.preempts"))
            );
            assert_eq!(
                std::fs::read(&users).unwrap(),
                before,
                "a trial wrote into {option}'s file"
            );
        }
    }

    /// Under `--no-namespace`, every record analyze and bisect write for a
    /// trial states the guest IDs every trial takes, resolved before the first
    /// (`RunOpts::prepare_trial_guest_ids`, `crate::app::run::resolved_trial_guest_ids`):
    /// the schedule a bisection or an analysis replays (`write_trial_schedule`),
    /// a prefix record (`write_trial_record`), a minimization candidate
    /// (`stamp_trial_record`) and the preemption record a check replays
    /// (`RunData::with_preempts_in`). A record without them reads as numbered
    /// by a PID namespace, which no such trial can replay. The input was
    /// recorded in a window Hermit chose (`refuse_host_ids: false`) and the
    /// trials refuse host IDs, so a written record states the trials' IDs only
    /// where it is stamped. Resolving the IDs probes the host, so this test
    /// sets them as resolution leaves them; the CLI tests
    /// `no_namespace_bisect_replays_the_schedules_it_writes` and
    /// `no_namespace_minimize_replays_its_candidate_records` run the whole
    /// path where the host admits chosen IDs.
    #[test]
    fn every_record_written_for_a_trial_states_the_trials_chosen_ids() {
        use detcore_model::pid::GuestIds;
        let trials = GuestIds::Chosen {
            base: 31744,
            refuse_host_ids: true,
        };
        let _resolved = crate::app::run::resolve_trial_guest_ids_in_test(trials);
        // The native reserved-input layout a recording made here carries,
        // which main's Chosen-input admission requires of every source.
        let provenance = serde_json::json!({
            "contract": "sealed-private-input-v1",
            "base": "0x71010000",
            "abi": std::env::consts::ARCH,
            "page_bytes": unsafe { libc::sysconf(libc::_SC_PAGESIZE) },
        });
        let with_provenance = |record: PreemptionRecord| -> PreemptionRecord {
            let mut value = serde_json::to_value(record).unwrap();
            value["reserved_input"] = provenance.clone();
            serde_json::from_value(value).unwrap()
        };
        let events = vec![SchedEvent::branches(detcore::types::DetTid::from_raw(3), 4)];
        let input = with_provenance(
            PreemptionRecord::from_sched_events(events.clone())
                .with_current_format()
                .with_guest_ids(GuestIds::Chosen {
                    base: 31744,
                    refuse_host_ids: false,
                }),
        );
        let args =
            crate::app::Args::try_parse_from(["hermit", "analyze", "--", "/bin/true"]).unwrap();
        let crate::app::Subcommand::Analyze(mut options) = args.command else {
            panic!("not analyze")
        };
        options.apply_global(&args.global);
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        // As analyze and bisect remember their input recordings.
        options.time_skip_sources.lock().unwrap().remember(&input);
        let states_the_trials_ids = |path: &Path| {
            assert_eq!(
                detcore::preemptions::read_recorded_guest_ids(path).unwrap(),
                trials,
                "{}",
                path.display()
            );
            assert_eq!(
                detcore::preemptions::read_recorded_reserved_input(path)
                    .unwrap()
                    .as_json(),
                Some(&provenance),
                "{}",
                path.display()
            );
        };
        // As the bisection writes a trial's schedule (`event_level_search`).
        let schedule = workspace.path().join("bisect_round_001.preempts");
        options
            .write_trial_schedule_from(events.clone(), &[], &schedule)
            .unwrap();
        states_the_trials_ids(&schedule);
        // As the baseline search writes a prefix of the target's record.
        let prefix = workspace.path().join("prefix.preempts");
        options.write_trial_record(input.clone(), &prefix).unwrap();
        states_the_trials_ids(&prefix);
        // As minimization builds a candidate from the initial schedule.
        let candidate = options.stamp_trial_record(with_provenance(
            PreemptionRecord::from_vecs(&Default::default())
                .with_guest_ids(input.guest_ids().unwrap_or_default()),
        ));
        assert_eq!(candidate.guest_ids(), Some(trials));
        // As the check of the critical preemption builds its run.
        let check = RunData::new_baseline(&options, "verify".to_owned())
            .unwrap()
            .with_preempts_in(with_provenance(PreemptionRecord::from_sched_events(events)));
        states_the_trials_ids(
            check
                .runopts
                .det_opts
                .det_config
                .replay_preemptions_from
                .as_deref()
                .unwrap(),
        );
    }
    /// `--trial-timeout=auto` bounds a run at max(60 s, 10 times the
    /// slowest endpoint run measured so far), rounded up to whole seconds,
    /// and gives no bound before an endpoint was measured; a number of
    /// seconds bounds every run but the target run, and `off` none.
    #[test]
    fn the_trial_bound_derives_from_the_slowest_endpoint_run() {
        use std::time::Duration;
        let options = |extra: &[&str]| {
            let mut argv = vec!["analyze"];
            argv.extend(extra);
            argv.extend(["--", "/bin/true"]);
            AnalyzeOpts::try_parse_from(argv).unwrap()
        };
        let mut auto = options(&[]);
        let trial = RunData::get_raw_runopts(&auto);
        assert_eq!(auto.trial_bound(&trial), None, "nothing measured yet");
        assert!(auto.endpoints.record(Duration::from_secs(2), None));
        assert_eq!(auto.trial_bound(&trial), NonZeroU64::new(60), "the floor");
        assert!(auto.endpoints.record(Duration::from_millis(30_150), None));
        assert_eq!(auto.trial_bound(&trial), NonZeroU64::new(302));
        assert!(
            !auto.endpoints.record(Duration::from_secs(1), None),
            "a faster endpoint changes nothing"
        );
        assert_eq!(auto.trial_bound(&trial), NonZeroU64::new(302));
        auto.endpoints.record(Duration::MAX, None);
        assert_eq!(auto.trial_bound(&trial), NonZeroU64::new(u64::MAX));

        let explicit = options(&["--trial-timeout=5"]);
        assert_eq!(
            explicit.trial_bound(&trial),
            NonZeroU64::new(5),
            "an explicit bound needs no measurement"
        );
        let mut off = options(&["--trial-timeout=off"]);
        off.endpoints.record(Duration::from_secs(30), None);
        assert_eq!(off.trial_bound(&trial), None);
        for refused in ["--trial-timeout=0", "--trial-timeout=soon"] {
            let error = AnalyzeOpts::try_parse_from(["analyze", refused, "--", "/bin/true"])
                .expect_err(refused);
            assert!(
                error.to_string().contains("positive number of seconds"),
                "{refused}: {error}"
            );
        }
    }

    /// A run created after an endpoint run was measured runs under the
    /// bound, or under the run arguments' own `--timeout` where that is
    /// shorter, and so does its printed reproducer; the target run is not
    /// bounded.
    #[test]
    fn runs_take_the_bound_and_the_target_run_does_not() {
        let created = |run_args: &[&str], explicit: Option<&str>| {
            let mut argv = vec!["analyze"];
            argv.extend(explicit);
            argv.push("--");
            argv.extend(run_args);
            argv.push("/bin/true");
            let mut options = AnalyzeOpts::try_parse_from(argv).unwrap();
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            options
                .endpoints
                .record(std::time::Duration::from_secs(10), None);
            let trial = RunData::new_baseline(&options, "bounded".to_owned()).unwrap();
            let target = RunData::new_run1_target(&options, "target".to_owned()).unwrap();
            let seconds = |run: &RunData| run.runopts.run_timeout().map(|bound| bound.as_secs());
            assert_eq!(
                trial.to_repro().contains(" --timeout="),
                seconds(&trial).is_some(),
                "{}",
                trial.to_repro()
            );
            (seconds(&trial), seconds(&target), trial.trial_bound)
        };
        assert_eq!(created(&[], None), (Some(100), None, NonZeroU64::new(100)));
        assert_eq!(
            created(&[], Some("--trial-timeout=5")),
            (Some(5), None, NonZeroU64::new(5)),
            "an explicit trial bound also leaves the initial target attempt alone"
        );
        assert_eq!(created(&["--timeout=30"], None), (Some(30), Some(30), None));
        assert_eq!(
            created(&["--timeout=300"], None),
            (Some(100), Some(300), NonZeroU64::new(100))
        );
        assert_eq!(
            created(&[], Some("--trial-timeout=off")),
            (None, None, None)
        );
    }

    /// A run that the bound ended says which bound it was and how to change
    /// it; one that its own `--timeout` ended, or that failed otherwise, is
    /// left as it was.
    #[test]
    fn a_run_the_trial_bound_ends_names_the_bound() {
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        options
            .endpoints
            .record(std::time::Duration::from_secs(1), None);
        let run = RunData::new_baseline(&options, "run2_baseline".to_owned()).unwrap();
        let timed_out = run.name_the_trial_bound(anyhow::Error::new(RunTimeoutMarker));
        assert!(timed_out.downcast_ref::<RunTimeoutMarker>().is_some());
        let message = format!("{timed_out:#}");
        assert!(
            message.starts_with(
                "run2_baseline reached hermit analyze's bound of 60 s per run \
                 (--trial-timeout=SECONDS raises it, --trial-timeout=off lifts it)"
            ),
            "{message}"
        );
        let other = run.name_the_trial_bound(anyhow::anyhow!("no such file"));
        assert_eq!(format!("{other:#}"), "no such file");
    }

    /// While trials are bounded, a trial's log file is capped at
    /// max(64 MiB, 10 times the largest endpoint log); before an endpoint
    /// wrote one, or with `--trial-timeout=off`, by HERMIT_LOG_MAX_BYTES alone.
    #[test]
    fn a_trials_log_is_capped_by_the_largest_endpoint_log() {
        use std::time::Duration;
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        assert_eq!(options.trial_log_limit(), None);
        options
            .endpoints
            .record(Duration::from_secs(1), Some(1 << 20));
        assert_eq!(options.trial_log_limit(), Some(64 << 20));
        options
            .endpoints
            .record(Duration::from_secs(1), Some(100 << 20));
        assert_eq!(options.trial_log_limit(), Some(1000 << 20));
        options
            .endpoints
            .record(Duration::from_secs(1), Some(2 << 20));
        assert_eq!(options.trial_log_limit(), Some(1000 << 20));
        let trial = RunData::new_baseline(&options, "bounded_log".to_owned()).unwrap();
        assert_eq!(trial.trial_global_opts().log_file_limit, Some(1000 << 20));
        options.trial_timeout = TrialTimeout::Off;
        assert_eq!(options.trial_log_limit(), None);
        let trial = RunData::new_baseline(&options, "unbounded_log".to_owned()).unwrap();
        assert_eq!(trial.trial_global_opts().log_file_limit, None);
    }

    /// An explicit `--trial-timeout` that the runs cannot keep (KVM, which
    /// does not enforce `--timeout`, and the in-guest runtime, which refuses
    /// one) is refused before any workspace or run; `auto` does without it.
    #[test]
    fn an_unenforceable_trial_timeout_is_refused_and_auto_does_without() {
        let options = |argv: &[&str]| {
            let args = crate::app::Args::try_parse_from(argv).unwrap();
            let crate::app::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options
        };
        let mut backends = vec!["kvm"];
        if cfg!(feature = "liteinst") {
            backends.push("liteinst");
        }
        for backend in backends {
            let backend = format!("--backend={backend}");
            let explicit = options(&[
                "hermit",
                &backend,
                "analyze",
                "--trial-timeout=5",
                "--",
                "/bin/true",
            ]);
            let error = explicit
                .refuse_unenforceable_trial_timeout()
                .expect_err("the runs cannot keep a --timeout");
            assert!(
                error
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
                    .is_some(),
                "{backend}: {error:#}"
            );
            assert!(error.to_string().contains("--trial-timeout=5"), "{error:#}");

            let mut auto = options(&["hermit", &backend, "analyze", "--", "/bin/true"]);
            auto.refuse_unenforceable_trial_timeout().unwrap();
            auto.endpoints
                .record(std::time::Duration::from_secs(10), None);
            assert_eq!(
                auto.trial_bound(&RunData::get_raw_runopts(&auto)),
                None,
                "{backend}"
            );
        }
        options(&["hermit", "analyze", "--trial-timeout=5", "--", "/bin/true"])
            .refuse_unenforceable_trial_timeout()
            .unwrap();
    }
    /// The stack-trace replay the report takes must give the observed
    /// completed predicate, including a guest's nonzero exit, even when neither
    /// diagnostic stack is available: the report compares that outcome with
    /// the search's, and ends the analysis when they differ.
    #[test]
    fn final_replays_require_the_observed_completed_outcome() {
        use reverie::process::ExitStatus;

        let dir = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(dir.path().to_path_buf());
        for actual in [false, true] {
            let replay = options
                .replay_for_stacks(
                    "completed_final",
                    &dir.path().join("candidate.json"),
                    &[],
                    1,
                    |run| {
                        assert!(!run.has_launched());
                        let output = Output {
                            status: ExitStatus::Exited(if actual { 122 } else { 0 }),
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        };
                        // Model the completed observation made by launch. This
                        // module can access the private state without adding a
                        // product API that could manufacture completion.
                        run.is_a_match = Some(options.output_matches(&output));
                        assert_eq!(run.is_a_match(), actual);
                        // A completed replay records the schedule it ran.
                        PreemptionRecord::from_sched_events(Vec::new())
                            .write_to_disk(run.sched_path_out())
                            .map_err(anyhow::Error::msg)?;
                        Ok(())
                    },
                )
                .expect("a completed replay gives its outcome");
            assert_eq!(replay.met_target, actual);
        }
    }

    #[test]
    fn only_completed_runs_contribute_endpoint_measurements() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(dir.path().to_path_buf());
        let mut run = RunData::new_run1_target(&options, "initial".to_owned()).unwrap();
        assert_eq!(run.measurement(), None);
        // Even a partial timing measurement must not make an unfinished run
        // eligible. Both completed predicates can contribute measurements.
        run.wall = Some(Duration::from_secs(10));
        options.record_endpoint(&run);
        assert_eq!(options.endpoints.slowest, None);
        run.is_a_match = Some(false);
        options.record_endpoint(&run);
        assert_eq!(options.endpoints.slowest, Some(Duration::from_secs(10)));
        let earlier = RunData::new_baseline(&options, "earlier".to_owned()).unwrap();
        run.is_a_match = Some(true);
        run.wall = Some(Duration::from_secs(20));
        options.record_endpoint(&run);
        assert_eq!(options.endpoints.slowest, Some(Duration::from_secs(20)));
        let later = RunData::new_baseline(&options, "later".to_owned()).unwrap();
        assert_eq!(
            earlier.runopts.run_timeout(),
            Some(Duration::from_secs(100))
        );
        assert_eq!(later.runopts.run_timeout(), Some(Duration::from_secs(200)));
    }

    #[test]
    fn continuation_admission_uses_the_resolved_range_maximum() {
        for (range, continuation, admitted) in [
            ("1000..5000000", 5_000_000, true),
            ("1000..5000000", 4_999_999, false),
            ("1000..500000000", 500_000_000, true),
            ("1000..500000000", 200_000_000, false),
        ] {
            let options = AnalyzeOpts::try_parse_from([
                "analyze".to_owned(),
                "--prefix-search".into(),
                format!("--continuation-timeslice={continuation}"),
                "--".into(),
                "--chaos".into(),
                format!("--chaos-timeslice-range={range}"),
                "/bin/true".into(),
            ])
            .unwrap();
            let result = options.refuse_unsound_continuation_timeslice();
            if admitted {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                assert!(
                    error
                        .downcast_ref::<crate::app::container::PolicyRefusal>()
                        .is_some(),
                    "{error:#}"
                );
                assert!(
                    format!("{error:#}").contains("shorter than the run's --max-timeslice="),
                    "{error:#}"
                );
            }
        }
        let seeded = AnalyzeOpts::try_parse_from([
            "analyze",
            "--prefix-search",
            "--",
            "--chaos",
            "--chaos-seeded-timeslice",
            "--seed=7",
            "/bin/true",
        ])
        .unwrap();
        assert_eq!(
            seeded.trial_max_timeslice().unwrap(),
            Some(hermit::chaos_timeslice::DEFAULT_MAX_TIMESLICE)
        );
    }

    /// A `--continuation-timeslice` shorter than the run's `--max-timeslice`,
    /// or one for a run without PMU timeslices, is refused before any
    /// workspace or trial, naming what to pass instead; one at least as long
    /// is accepted.
    #[test]
    fn a_continuation_timeslice_that_would_change_the_prefix_is_refused() {
        let refusal = |argv: &[&str]| {
            let mut full = vec!["analyze", "--prefix-search"];
            full.extend(argv);
            full.push("/bin/true");
            AnalyzeOpts::try_parse_from(full)
                .unwrap()
                .refuse_unsound_continuation_timeslice()
        };
        for (argv, reason) in [
            (
                &[
                    "--continuation-timeslice=1000",
                    "--",
                    "--max-timeslice=100000",
                ][..],
                "shorter than the run's --max-timeslice=100000",
            ),
            (
                &["--continuation-timeslice=20000000", "--"][..],
                "shorter than the run's --max-timeslice=200000000",
            ),
            (
                &[
                    "--continuation-timeslice=1000",
                    "--",
                    "--max-timeslice=disabled",
                ][..],
                "disable PMU timeslices",
            ),
        ] {
            let error = refusal(argv).expect_err(reason);
            assert!(
                error
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
                    .is_some(),
                "{error:#}"
            );
            let message = format!("{error:#}");
            assert!(
                message.contains(reason) && message.contains("Omit it (auto)"),
                "{argv:?}: {message}"
            );
        }
        refusal(&[
            "--continuation-timeslice=200000000",
            "--",
            "--max-timeslice=1000",
        ])
        .unwrap();
        refusal(&["--"]).unwrap();
    }

    /// A bisection trial's schedules are removed once read, unless --keep-tmp
    /// keeps them: the workspace's names only, so a schedule the run arguments
    /// name (`--replay-schedule-from`) stays, and so does the reproducer.
    #[test]
    fn a_trials_schedules_are_discarded_unless_kept() {
        for keep_tmp in [false, true] {
            let outside = tempfile::tempdir().unwrap();
            let user_schedule = outside.path().join("mine.preempts");
            // Main's trials read the record their run arguments replay when
            // they are built (`RunData::new_baseline`), so it exists first.
            PreemptionRecord::from_sched_events(Vec::new())
                .write_to_disk(&user_schedule)
                .unwrap();
            let replay = format!("--replay-schedule-from={}", user_schedule.display());
            let mut options =
                AnalyzeOpts::try_parse_from(["analyze", "--", replay.as_str(), "/bin/true"])
                    .unwrap();
            options.keep_tmp = keep_tmp;
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            let run = RunData::new_baseline(&options, "bisect_round_001".to_owned()).unwrap();
            let root = run.root_path();
            let written = [
                root.with_extension(PREEMPTS_EXT),
                run.out_path().with_extension(PREEMPTS_EXT),
                run.timed_recording_path(),
            ];
            // They are the names a trial built as the bisection builds one,
            // without a schedule in its run arguments, replays and records.
            let mut plain = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
            plain.tmp_dir = options.tmp_dir.clone();
            let mut trial = RunData::new_baseline(&plain, "bisect_round_001".to_owned())
                .unwrap()
                .with_schedule_replay()
                .with_schedule_recording();
            assert_eq!(trial.sched_path_in(), written[0]);
            assert_eq!(trial.sched_path_out(), written[1]);
            let kept = [root.with_extension("repro"), user_schedule.clone()];
            for path in written.iter().chain(&kept) {
                std::fs::write(path, "{}").unwrap();
            }
            run.discard_schedules();
            for path in &written {
                assert_eq!(
                    path.exists(),
                    keep_tmp,
                    "{} with keep_tmp={keep_tmp}",
                    path.display()
                );
            }
            for path in &kept {
                assert!(path.exists(), "{} was removed", path.display());
            }
            // An unfinished trial recorded no schedule: what it wrote goes.
            std::fs::write(&written[2], "{}").unwrap();
            run.discard_schedules();
            assert_eq!(written[2].exists(), keep_tmp);
        }
    }

    /// The normalized copy of a run's preemption record is written beside it
    /// for reading, and the run keeps its own record: the target endpoint
    /// and --minimize's baseline replay the record that met the target.
    /// Before, the run switched to the copy, whose renumbered priorities can
    /// run another schedule, and a minimized record that met the target was
    /// then "still not matching" it.
    #[test]
    fn a_normalized_record_is_written_for_reading_and_not_replayed() {
        use detcore::preemptions::PreemptionRecord;
        let workspace = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let recorded = workspace.path().join("round_014.preempts");
        let history = vec![
            (detcore::types::LogicalTime::from_nanos(1_000), 352),
            (detcore::types::LogicalTime::from_nanos(2_000), 4923),
        ];
        let record = PreemptionRecord::from_vecs(&std::collections::BTreeMap::from([(
            detcore::DetTid::from_raw(7),
            history,
        )]));
        record.write_to_disk(&recorded).unwrap();
        let mut run = RunData::from_minimize_output(
            &options,
            "minimized".to_owned(),
            RunData::get_raw_runopts(&options),
            record.clone(),
            recorded.clone(),
            workspace.path().join("round_014.log"),
        );
        let (normalized_path, normalized) = run.write_normalized_preempts();
        assert_eq!(
            normalized_path,
            workspace.path().join("round_014.normalized")
        );
        assert!(normalized_path.exists());
        assert_ne!(normalized, record, "normalizing renumbers the priorities");
        assert_eq!(run.preempts_path_out(), recorded.as_path());
        assert_eq!(run.preempts_out(), &record);
    }
}

#[cfg(test)]
mod replay_departure_tests {
    use clap::Parser;

    use super::*;
    use crate::app::container::PolicyRefusal;

    #[test]
    fn analysis_refuses_marked_inputs_and_outputs_before_discarding_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("raw.preempts");
        let output = directory.path().join("stripped.preempts");
        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let mut event = SchedEvent::branches(detcore::types::DetTid::from_raw(3), 2);
        event.end_time = Some(detcore::types::LogicalTime::from_nanos(12));
        for marker in [None, Some(0)] {
            let record = PreemptionRecord::from_sched_events(vec![event.clone()])
                .retain_replay_departure(marker);
            record.write_to_disk(&input).unwrap();
            let raw = std::fs::read(&input).unwrap();
            std::fs::write(&output, b"existing destination").unwrap();
            let admitted = options.refuse_recordings_under_another_pipe_wake_latency(&[(
                "--run1-preemptions",
                &input,
            )]);
            let processed = RunData::post_process_events_file(&input, &output);
            if marker.is_some() {
                for result in [admitted, processed] {
                    let error = result.expect_err("known omissions cannot supply trial evidence");
                    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
                    assert!(format!("{error:#}").contains("HERMIT_INCOMPLETE_REPLAY_RECORD"));
                }
                assert_eq!(std::fs::read(&output).unwrap(), b"existing destination");
            } else {
                admitted.unwrap();
                processed.unwrap();
                let actual = PreemptionReader::try_new(&output).unwrap().into_inner();
                let mut expected_event = event.clone();
                expected_event.end_time = None;
                let expected = PreemptionRecord::from_sched_events(vec![expected_event]);
                assert_eq!(actual, expected);
            }
            assert_eq!(
                std::fs::read(&input).unwrap(),
                raw,
                "retain raw diagnostics"
            );
        }
    }
}
