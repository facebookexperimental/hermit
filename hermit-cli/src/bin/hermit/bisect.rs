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
use detcore::clock_identity::combined_identity;
#[cfg(test)]
use detcore::preemptions::HistoryIdentity;
use detcore::preemptions::PreemptionRecord;
use hermit::Error;
use regex::Regex;
use reverie::process::ExitStatus;
use tracing::metadata::LevelFilter;

use super::analyze::AnalyzeOpts;
use super::analyze::ExitStatusConstraint;
use super::analyze::MemoryBudget;
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

    /// Exit code identifying the target failure, 0 to 255, as a shell reports it: the guest's own,
    /// or 128 plus the signal number when a signal killed it.
    #[clap(long, default_value = "nonzero", value_name = "NUM|nonzero|any")]
    target_exit_code: ExitStatusConstraint,

    /// Logging level for replayed guest runs. Above warn, each replay writes its log into a
    /// file of its own in the workspace.
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

    /// The most memory bisect's own process may hold, as `hermit analyze
    /// --max-memory` takes it: a size, `auto` (a quarter of the memory it may
    /// use), or `off`.
    #[clap(long, value_name = "SIZE|auto|off", default_value = "auto")]
    max_memory: MemoryBudget,

    /// Arguments for the underlying `hermit run`, followed by the program and its arguments.
    #[clap(value_name = "RUN_ARGS", required = true)]
    run_args: Vec<String>,

    /// Why this host's retired-branch counter is inexact, if it is (see
    /// `AnalyzeOpts::inexact_branch_counter`).
    #[clap(skip = crate::app::host_capabilities::host_inexact_branch_counter as fn() -> Option<String>)]
    inexact_branch_counter: fn() -> Option<String>,
}

/// What [`BisectOpts::prepare`] gives the first replay: the trial
/// configuration and the good and bad schedules (each stating the RCB
/// coordinate identity both were checked to share).
type Prepared = (AnalyzeOpts, PreemptionRecord, PreemptionRecord);

impl BisectOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // Before the two schedules are read, the largest allocation.
        let _watchdog = self.max_memory.enforce("hermit bisect")?;
        let (mut analyzer, good, bad) = self.prepare(global)?;
        analyzer.bisect_schedule_pair(good, bad)
    }

    /// Everything `main` does before the first replay: the refusals, the two
    /// schedules, and the trial configuration they imply.
    fn prepare(&self, global: &GlobalOpts) -> Result<Prepared, Error> {
        let mut analyzer = AnalyzeOpts {
            target_stdout: self.target_stdout.clone(),
            target_stderr: self.target_stderr.clone(),
            target_exit_code: self.target_exit_code.clone(),
            guest_log: self.guest_log,
            // Bisect keeps its workspace whatever its log level.
            guest_log_from_env: false,
            selfcheck: false,
            search: false,
            run_needleman: self.needleman,
            minimize: false,
            prefix_search: false,
            continuation_timeslice: crate::app::analyze::ContinuationTimeslice::Auto,
            imprecise_search: false,
            replays_instruction_offsets: false,
            run1_seed: None,
            run1_preemptions: None,
            run1_schedule: None,
            run2_seed: None,
            run2_preemptions: None,
            run2_schedule: None,
            report_file: self.report_file.clone(),
            // The "Racing accesses" section is analyze's: bisect's report
            // keeps its two stacks and runs no extra traced replay.
            no_racing_accesses: true,
            race_window: 0,
            analyze_seed: None,
            verbose: self.verbose,
            // The replays log at --guest-log's level (warn without it).
            trial_log: None,
            jitter_dist: self.jitter_dist,
            execution_context: self.execution_context,
            tmp_dir: None,
            max_memory: self.max_memory,
            // Bisect keeps its workspace (`bisect_schedule_pair`), and every
            // trial's schedules in it, as it has no --keep-tmp to ask.
            keep_tmp: true,
            success_exit_code: None,
            run_arg: Vec::new(),
            run_args: self.run_args.clone(),
            // Replays run on the globally selected backend
            // (`hermit --backend <BACKEND> bisect ...`) and charge the
            // invocation's shared `--max-log-bytes` budget.
            backend: global.backend,
            max_log_bytes: global.max_log_bytes,
            log_budget: global.log_budget(),
            inexact_branch_counter: self.inexact_branch_counter,
            branch_counter_warning: None,
            // Bisect runs no endpoint to measure: its replays are bounded by
            // the run arguments' --timeout alone.
            trial_timeout: crate::app::analyze::TrialTimeout::Off,
            endpoints: Default::default(),
            time_skip_sources: Default::default(),
            // Bisect has no budget and no journal: its replays run as they
            // did before analyze had them.
            time_budget: crate::app::analyze::TimeBudget::Off,
            trial_budget: crate::app::analyze::TrialBudget::Off,
            resume: None,
            budget: Default::default(),
            journal: Default::default(),
            progress: Default::default(),
            analyze_seed_drawn: false,
            partial_report_written: false,
            resumed_progress: None,
            trial_runner: Default::default(),
            hermit_run: Default::default(),
            trial_schedules_uncapped: Default::default(),
        };
        // Before the schedules are read: a refused cap or counter reads and
        // starts nothing.
        analyzer.refuse_unsupervised_log_cap()?;
        analyzer.refuse_options_trials_do_not_apply()?;
        analyzer.refuse_outputs_trials_overwrite()?;
        analyzer.refuse_unqualified_trial_timeout()?;
        analyzer.install_trial_pmu_config()?;
        analyzer.admit_branch_counter(&mut crate::app::tracing::write_stderr_diagnostic)?;
        if let Some(notice) =
            analyzer.small_trial_max_timeslice_notice(crate::app::run::installed_skid_margin)
        {
            crate::app::tracing::write_stderr_diagnostic(&notice);
        }

        // A schedule of a format this Hermit does not read is refused before
        // anything else is read from either, as a policy refusal: main's
        // `read_schedule` reports a failed validation as an untyped error.
        analyzer.refuse_unread_record_formats(&[("--good", &self.good), ("--bad", &self.bad)])?;
        let good = read_schedule(&self.good, "good")?;
        let bad = read_schedule(&self.bad, "bad")?;
        AnalyzeOpts::refuse_recordings_numbered_apart(&[
            ("--good", &self.good),
            ("--bad", &self.bad),
        ])?;
        // Trials start through `RunOpts::run`, which does not read the epoch
        // a replayed record carries, so the trials replay from the one the
        // two schedules were recorded under.
        analyzer.adopt_recorded_epoch(&[("--good", &self.good), ("--bad", &self.bad)])?;
        // Every replay charges expired timed waits as the failing schedule did.
        analyzer.adopt_expired_wait_charge(Some(&self.bad))?;
        // Every replay registers queued sleeps as the failing schedule did.
        analyzer.adopt_sleep_registration(Some(&self.bad))?;
        // Keep the timeslice drawn by the recordings.
        analyzer.adopt_recorded_timeslice(&[("--good", &self.good), ("--bad", &self.bad)])?;
        analyzer.refuse_recordings_under_another_pipe_wake_latency(&[
            ("--good", &self.good),
            ("--bad", &self.bad),
        ])?;
        analyzer
            .refuse_unadmitted_rcb_histories(&[("--good", &self.good), ("--bad", &self.bad)])?;
        // Every trial schedule mixes the two, so both must count the same
        // thing; the records keep the identity they state.
        combined_identity(
            (&format!("--good {}", self.good.display()), good.identity()),
            (&format!("--bad {}", self.bad.display()), bad.identity()),
        )
        .map_err(|refusal| {
            anyhow::Error::new(crate::app::container::PolicyRefusal).context(refusal)
        })?;
        // A trial spliced from either schedule's prefix makes the jumps of
        // global time that prefix contains (`crate::app::time_skip_carry`).
        {
            let mut sources = analyzer.time_skip_sources.lock().unwrap();
            sources.remember(&good);
            sources.remember(&bad);
            // Refuse before the first trial, rather than silently downgrading
            // a format-3 prefix's charging rule to a legacy/format-2 rule.
            sources.trial_format()?;
            sources.validate_reserved_inputs()?;
        }
        analyzer.adopt_instruction_offsets(&[("--good", &self.good), ("--bad", &self.bad)])?;
        // Every replay's schedule is written anew, stamped with the replays'
        // guest IDs, so they are resolved before the first.
        analyzer.prepare_trial_guest_ids(&[("--good", &self.good), ("--bad", &self.bad)])?;
        analyzer.adopt_uncapped_slices(&[self.good.as_path(), self.bad.as_path()])?;
        let (mut good, mut bad) = (good, bad);
        // Analyze's trial outputs omit optional event timestamps. Put these
        // admitted search inputs in that same representation: bisection edits
        // event order, not the original recordings' virtual-time trajectories.
        // Stripping event times keeps the records' callback checkpoints valid.
        for event in good
            .schedevents_iter_mut()
            .chain(bad.schedevents_iter_mut())
        {
            event.end_time = None;
        }
        let same_events = good.schedevents().len() == bad.schedevents().len()
            && good
                .schedevents()
                .iter()
                .zip(bad.schedevents())
                .all(|(g, b)| g.same_operations(b));
        if same_events {
            // An input bisect cannot use, not a failure of Hermit.
            return Err(
                crate::app::analyze::AnalyzePrecondition::endpoints_identical(
                    "the --good and --bad schedules contain identical event traces",
                )
                .into(),
            );
        }
        Ok((analyzer, good, bad))
    }
}

fn read_schedule(path: &Path, label: &str) -> anyhow::Result<PreemptionRecord> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read --{label} schedule {}", path.display()))?;
    let record: PreemptionRecord =
        detcore::preemptions::boundary_recording::Recording::decode(contents.as_bytes())
            .and_then(|recording| recording.into_legacy("bisect"))
            .with_context(|| format!("failed to parse --{label} schedule {}", path.display()))?;
    record
        .validate()
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("--{label} schedule {} failed validation", path.display()))?;
    record.require_faithful_replay().map_err(|error| {
        anyhow::Error::new(crate::app::container::PolicyRefusal).context(format!(
            "--{label} schedule {} cannot supply bisection evidence: {error:#}",
            path.display()
        ))
    })?;
    if !record.contains_schedevents() {
        bail!(
            "--{label} schedule {} contains no global schedule events",
            path.display()
        );
    }
    Ok(record)
}

#[cfg(test)]
mod tests {

    /// Bisect accepts schedules whose events carry instruction offsets, keeps
    /// them in the events it searches, and replays its trials without chaos
    /// (`AnalyzeOpts::adopt_instruction_offsets`) and without drawing
    /// offsets of its own. A schedule of a newer format is refused by name,
    /// before anything else is read from it (here its epoch, which the run
    /// arguments' would contradict).
    #[test]
    fn bisect_replays_offset_schedules_without_chaos() {
        let tid = DetTid::from_raw(3);
        let offset = detcore::preemptions::InstructionOffset {
            rcb_target: 18,
            instructions: 4,
        };
        let batch =
            |count, offset| SchedEvent::branches(tid, count).with_instruction_offset(offset);
        // A recording without offsets states main's format below the offsets
        // format, the timing format the recording with them shares.
        let mut good = search_record(vec![batch(20, None), batch(5, None)]);
        good["format_version"] =
            serde_json::json!(detcore::preemptions::INSTRUCTION_OFFSET_ROWS_FORMAT - 1);
        let bad = search_record(vec![batch(22, Some(offset)), batch(3, None)]);
        assert_eq!(
            bad["format_version"],
            detcore::preemptions::INSTRUCTION_OFFSET_FORMAT,
            "{bad}"
        );
        let dir = tempfile::tempdir().unwrap();
        let (good_path, bad_path) = (dir.path().join("good.json"), dir.path().join("bad.json"));
        fs::write(&good_path, good.to_string()).unwrap();
        fs::write(&bad_path, bad.to_string()).unwrap();
        let prepare = |bad_path: &Path| {
            let args = crate::app::Args::try_parse_from([
                "hermit".to_owned(),
                "bisect".to_owned(),
                format!("--good={}", good_path.display()),
                format!("--bad={}", bad_path.display()),
                "--".to_owned(),
                format!("--epoch={RECORDED}"),
                "--pipe-wake-latency=3000000".to_owned(),
                "--chaos".to_owned(),
                "/bin/true".to_owned(),
            ])
            .unwrap();
            let crate::app::Subcommand::Bisect(options) = args.command else {
                panic!("expected bisect")
            };
            options.prepare(&args.global)
        };
        let (mut analyzer, _, bad_events) = prepare(&bad_path).unwrap();
        assert!(analyzer.replays_instruction_offsets);
        assert_eq!(bad_events.schedevents()[0].instruction_offset, Some(offset));
        analyzer.tmp_dir = Some(dir.path().to_path_buf());
        let raw = crate::app::analyze::rundata::RunData::raw_runopts(&analyzer);
        assert!(raw.det_opts.det_config.chaos);
        let trial = crate::app::analyze::rundata::RunData::new(&analyzer, "trial".to_owned(), raw)
            .with_schedule_replay_from(bad_path.clone())
            .into_runopts();
        assert!(!trial.det_opts.det_config.chaos);
        assert_eq!(trial.det_opts.det_config.chaos_instruction_offset, 0);

        // A schedule of a newer format is refused by name as a policy refusal,
        // with main's `validate_format` message, before its epoch is read.
        let newer_format = detcore::preemptions::PREEMPTION_RECORD_FORMAT_MAX + 1;
        let mut newer = bad.clone();
        newer["format_version"] = serde_json::json!(newer_format);
        newer["epoch"] = serde_json::json!("2001-02-03T04:05:06Z");
        let newer_path = dir.path().join("newer.json");
        fs::write(&newer_path, newer.to_string()).unwrap();
        let Err(error) = prepare(&newer_path) else {
            panic!("a newer format is refused")
        };
        assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
        assert!(
            format!("{error:#}").contains(&format!(
                "record format Some({newer_format}) with 0 time skips"
            )),
            "{error:#}"
        );
    }

    /// Endpoints that differ only in where an instruction-offset timer ended a
    /// batch are different event traces, so bisect searches between them;
    /// the same offset at both ends is refused as identical.
    #[test]
    fn bisect_tells_endpoints_apart_by_their_instruction_offsets() {
        let tid = DetTid::from_raw(3);
        let ended = |instructions| {
            SchedEvent::branches(tid, 8).with_instruction_offset(Some(
                detcore::preemptions::InstructionOffset {
                    rcb_target: 100,
                    instructions,
                },
            ))
        };
        let record =
            |instructions| search_record(vec![ended(instructions), SchedEvent::branches(tid, 4)]);
        let (_, good, bad) = prepare_search_records(&record(6), &record(7)).unwrap();
        assert_eq!(
            (
                good.schedevents()[0].instruction_offset,
                bad.schedevents()[0].instruction_offset
            ),
            (ended(6).instruction_offset, ended(7).instruction_offset)
        );
        let error = prepare_search_records(&record(6), &record(6)).unwrap_err();
        assert!(
            error.to_string().contains("identical event traces"),
            "{error:#}"
        );
    }
    use std::num::NonZeroUsize;

    use detcore::types::DetTid;
    use detcore::types::LogicalTime;
    use detcore::types::Op;
    use detcore::types::SchedEvent;
    use detcore::types::SyscallPhase;
    use reverie::syscalls::Sysno;

    use super::*;
    use crate::app::container::PolicyRefusal;

    fn search_record(events: Vec<SchedEvent>) -> serde_json::Value {
        let record = PreemptionRecord::from_sched_events_with_identity(events, known())
            .with_pipe_wake_latency(Some(3_000_000))
            .with_signal_state_copy(Some(true));
        let mut record: serde_json::Value = serde_json::from_str(&record.to_string()).unwrap();
        settings(&mut record)["epoch"] = serde_json::json!(RECORDED);
        record
    }

    /// Exercise production admission and conversion without launching a trial;
    /// both successful preparation and refusal must leave the inputs intact.
    fn prepare_search_records(
        good: &serde_json::Value,
        bad: &serde_json::Value,
    ) -> Result<Prepared, Error> {
        let dir = tempfile::tempdir().unwrap();
        let good_path = dir.path().join("good.json");
        let bad_path = dir.path().join("bad.json");
        let good_bytes = serde_json::to_vec(good).unwrap();
        let bad_bytes = serde_json::to_vec(bad).unwrap();
        fs::write(&good_path, &good_bytes).unwrap();
        fs::write(&bad_path, &bad_bytes).unwrap();
        let args = crate::app::Args::try_parse_from([
            "hermit".to_owned(),
            "bisect".to_owned(),
            format!("--good={}", good_path.display()),
            format!("--bad={}", bad_path.display()),
            "--".to_owned(),
            format!("--epoch={RECORDED}"),
            "--pipe-wake-latency=3000000".to_owned(),
            "/bin/true".to_owned(),
        ])
        .unwrap();
        let crate::app::Subcommand::Bisect(options) = args.command else {
            panic!("expected bisect")
        };
        let result = options.prepare(&args.global);
        assert_eq!(fs::read(good_path).unwrap(), good_bytes);
        assert_eq!(fs::read(bad_path).unwrap(), bad_bytes);
        result
    }

    /// Admission reaches the real source-format check before any trial.
    #[test]
    fn bisect_refuses_mixed_timing_formats_before_starting_a_trial() {
        let first = vec![SchedEvent::branches(DetTid::from_raw(3), 1)];
        let second = vec![SchedEvent::branches(DetTid::from_raw(5), 1)];
        let record = |events, format| {
            let mut value = search_record(events);
            if let Some(format) = format {
                settings(&mut value)["format_version"] = serde_json::json!(format);
            }
            value
        };
        for format in [None, Some(2), Some(3)] {
            prepare_search_records(
                &record(first.clone(), format),
                &record(second.clone(), format),
            )
            .unwrap();
        }
        for other in [None, Some(2)] {
            for formats in [[Some(3), other], [other, Some(3)]] {
                let error = prepare_search_records(
                    &record(first.clone(), formats[0]),
                    &record(second.clone(), formats[1]),
                )
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains("different timing formats"), "{message}");
                assert!(message.contains("record both inputs"), "{message}");
            }
        }
    }

    #[test]
    fn bisect_search_removes_only_event_times_from_both_inputs() {
        let first = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook)
            .with_start_rip(NonZeroUsize::new(0x1000).unwrap())
            .with_end_rip(NonZeroUsize::new(0x1002).unwrap());
        let second = SchedEvent::branches(DetTid::from_raw(5), 4);
        let good = vec![first, second];
        let mut timed_good = good.clone();
        timed_good[0].end_time = Some(LogicalTime::from_nanos(10));
        let trials_dir = tempfile::tempdir().unwrap();

        // Every distinction other than optional timestamps still reaches the
        // search, including the order and a genuinely unmatched event.
        for field in [
            "order",
            "thread",
            "op",
            "phase",
            "count",
            "start_rip",
            "end_rip",
            "extra",
        ] {
            let mut bad = good.clone();
            match field {
                "order" => bad.reverse(),
                "thread" => bad[0].dettid = DetTid::from_raw(7),
                "op" => bad[0].op = Op::Syscall(Sysno::read, SyscallPhase::Prehook),
                "phase" => bad[0].op = Op::Syscall(Sysno::write, SyscallPhase::Posthook),
                "count" => bad[1].count += 1,
                "start_rip" => bad[0].start_rip = NonZeroUsize::new(0x2000),
                "end_rip" => bad[0].end_rip = NonZeroUsize::new(0x2002),
                "extra" => bad.push(SchedEvent::branches(DetTid::from_raw(7), 1)),
                _ => unreachable!(),
            }
            let timed_bad = bad
                .iter()
                .cloned()
                .map(|event| event.with_time(LogicalTime::from_nanos(20)))
                .collect();
            let (mut trials, prepared_good, prepared_bad) = prepare_search_records(
                &search_record(timed_good.clone()),
                &search_record(timed_bad),
            )
            .unwrap();
            assert_eq!(prepared_good.schedevents(), &good, "{field}: good input");
            assert_eq!(prepared_bad.schedevents(), &bad, "{field}: bad input");
            assert_ne!(
                prepared_good.schedevents(),
                prepared_bad.schedevents(),
                "{field}"
            );
            trials.tmp_dir = Some(trials_dir.path().to_path_buf());
            assert_eq!(trials.trial_epoch_for_test(), RECORDED_RFC3339);
            assert_eq!(trials.trial_pipe_wake_latency(), Some(3_000_000));
            assert!(trials.trial_signal_state_copy());
        }
    }

    /// Two `--no-namespace` records whose guest IDs start from different
    /// bases are refused before any trial, as a policy refusal: every trial
    /// numbers its guest alike, so none could follow both. Records numbered
    /// alike are admitted where the trials number their guests that way too:
    /// here, by a PID namespace.
    #[test]
    fn bisect_refuses_records_whose_guests_were_numbered_apart() {
        use detcore_model::pid::GuestIds;
        let chosen = |base| {
            serde_json::to_value(GuestIds::Chosen {
                base,
                refuse_host_ids: true,
            })
            .unwrap()
        };
        // A record with chosen IDs carries the reserved-input layout of the
        // Hermit that made it, which main's Chosen-input admission requires of
        // every source (`ReservedInputMetadata::require_same_for_chosen`): the
        // native layout, as a recording made here would.
        let provenance = serde_json::json!({
            "contract": "sealed-private-input-v1",
            "base": "0x71010000",
            "abi": std::env::consts::ARCH,
            "page_bytes": unsafe { libc::sysconf(libc::_SC_PAGESIZE) },
        });
        let numbered = |count, ids: Option<serde_json::Value>| {
            let mut record = search_record(vec![SchedEvent::branches(DetTid::from_raw(3), count)]);
            if let Some(ids) = ids {
                settings(&mut record)["guest_ids"] = ids;
                settings(&mut record)["reserved_input"] = provenance.clone();
            }
            record
        };
        let error = prepare_search_records(
            &numbered(4, Some(chosen(31744))),
            &numbered(5, Some(chosen(40000))),
        )
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::app::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        assert!(
            error
                .to_string()
                .contains("Record both runs under --no-namespace with the same --pid-base"),
            "{error:#}"
        );
        let error = prepare_search_records(&numbered(4, Some(chosen(31744))), &numbered(5, None))
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::app::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("no trial could follow both"),
            "{error:#}"
        );
        // Numbered alike, but by chosen IDs, which these trials, each in a
        // PID namespace of its own, cannot follow.
        let error = prepare_search_records(
            &numbered(4, Some(chosen(31744))),
            &numbered(5, Some(chosen(31744))),
        )
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::app::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        assert!(
            error
                .to_string()
                .contains("numbers its guest by its own PID namespace"),
            "{error:#}"
        );
        prepare_search_records(&numbered(4, None), &numbered(5, None)).unwrap();
    }

    #[test]
    fn bisect_refuses_endpoints_differing_only_in_event_times() {
        let event = SchedEvent::branches(DetTid::from_raw(3), 4);
        for (good_time, bad_time) in [(Some(10), Some(20)), (Some(10), None), (None, Some(20))] {
            let mut good = event.clone();
            let mut bad = event.clone();
            good.end_time = good_time.map(LogicalTime::from_nanos);
            bad.end_time = bad_time.map(LogicalTime::from_nanos);
            // Ordinary event equality still distinguishes the original times.
            assert_ne!(good, bad);
            let error =
                prepare_search_records(&search_record(vec![good]), &search_record(vec![bad]))
                    .unwrap_err();
            assert!(
                error.to_string().contains("identical event traces"),
                "{error:#}"
            );
            assert_eq!(crate::app::failure_exit_code(&error), 2, "{error:#}");
            assert_eq!(
                crate::app::classify_failure(&error),
                "HERMIT_ANALYZE_PRECONDITION class=endpoints-identical"
            );
        }
    }

    #[test]
    fn bisect_admits_raw_records_before_projecting_search_times() {
        let event = SchedEvent::branches(DetTid::from_raw(3), 4);
        let good = search_record(vec![event.clone().with_time(LogicalTime::from_nanos(10))]);
        let bad = search_record(vec![event.with_time(LogicalTime::from_nanos(20))]);
        // These would become identical search vectors. The raw-record refusal
        // must win first, for either endpoint, and retain its original cause.
        for (field, value, cause, policy) in [
            (
                "per_thread",
                serde_json::json!({"3": {"final_prio": 0, "prio_changes": []}}),
                "final priority for thread 3 invalid: 0",
                false,
            ),
            (
                "epoch",
                serde_json::json!("2026-01-01T00:00:00Z"),
                "epoch",
                true,
            ),
            (
                "pipe_wake_latency",
                serde_json::json!(0),
                "--pipe-wake-latency=0",
                true,
            ),
            (
                "signal_state_copy",
                serde_json::json!(false),
                "--no-signal-state-copy",
                true,
            ),
            (
                "expired_wait_runs_charged_once",
                serde_json::json!(true),
                "--expired-wait-charge",
                true,
            ),
            (
                "queued_sleeps_registered_at_first_pass",
                serde_json::json!(true),
                "--sleep-registration",
                true,
            ),
        ] {
            for change_good in [true, false] {
                let mut good = good.clone();
                let mut bad = bad.clone();
                let changed = if change_good { &mut good } else { &mut bad };
                settings(changed)[field] = value.clone();
                let error = prepare_search_records(&good, &bad).unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains(cause), "{field}: {message}");
                assert!(
                    !message.contains("identical event traces"),
                    "{field}: {message}"
                );
                assert_eq!(
                    error.downcast_ref::<PolicyRefusal>().is_some(),
                    policy,
                    "{message}"
                );
            }
        }
    }

    /// Through `main`: bisect refuses `--strict` in its run arguments on an
    /// inexact retired-branch counter before it reads either schedule
    /// (https://github.com/rrnewton/hermit/issues/3810). The schedules do not
    /// exist, so a bisect that passed the refusal fails on reading them. With
    /// `--namespace-only` or `--lite`, which its replays ignore, bisect refuses
    /// earlier, for a dropped option
    /// (`bisect_refuses_run_options_its_replays_do_not_apply`).
    #[test]
    fn strict_bisect_refuses_an_inexact_branch_counter_before_reading_schedules() {
        let argv = [
            "hermit",
            "bisect",
            "--good=/nonexistent/good.json",
            "--bad=/nonexistent/bad.json",
            "--",
            "--strict",
            "/bin/true",
        ];
        let args = crate::app::Args::try_parse_from(argv).unwrap();
        let crate::app::Subcommand::Bisect(mut options) = args.command else {
            panic!("{argv:?} is not bisect")
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

        options.inexact_branch_counter = || None;
        let error = options.main(&args.global).unwrap_err();
        assert!(
            error.to_string().contains("failed to read --good schedule"),
            "{error:#}"
        );
    }

    /// Through `main`: bisect refuses run arguments that its replays would
    /// accept and run without (`RunOpts::options_only_main_applies`), before
    /// it reads either schedule. Without one, it reaches the missing
    /// schedule instead.
    #[test]
    fn bisect_refuses_run_options_its_replays_do_not_apply() {
        let bisect = |run_args: &[&str]| {
            let mut argv = vec![
                "hermit",
                "bisect",
                "--good=/nonexistent/good.json",
                "--bad=/nonexistent/bad.json",
                "--",
            ];
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = crate::app::Args::try_parse_from(&argv).unwrap();
            let crate::app::Subcommand::Bisect(options) = args.command else {
                panic!("{argv:?} is not bisect")
            };
            options.main(&args.global).unwrap_err()
        };
        for (run_args, named) in [
            (&["--namespace-only"][..], "--namespace-only"),
            (&["--lite"][..], "--namespace-only"),
            (&["--verify"][..], "--verify"),
            (&["--save-config=requested.config"][..], "--save-config"),
            (&["--summary-json=requested.summary"][..], "--summary-json"),
            (
                &["--record-networking=requested.trace"][..],
                "refuse --record-networking=requested.trace",
            ),
            (
                &["--strace-only"][..],
                "refuse --strace-only in the run arguments",
            ),
            (
                &["--no-sequentialize-threads"][..],
                "refuse --no-sequentialize-threads in the run arguments",
            ),
            (
                &["--network=host"][..],
                "refuse --network=host in the run arguments",
            ),
            (
                &["--no-namespace"][..],
                "refuse --no-namespace in the run arguments",
            ),
            (
                &["--gdbserver"][..],
                "refuse --gdbserver in the run arguments",
            ),
            (
                &["--dap-listen=127.0.0.1:0"][..],
                "refuse --dap-listen in the run arguments",
            ),
            (
                &["--record-preemptions-to=requested.preempts"][..],
                "refuse --record-preemptions-to in the run arguments",
            ),
            (
                &["--stacktrace-event=5,requested.stack"][..],
                "refuse --stacktrace-event with a path in the run arguments",
            ),
            (
                &["--preemption-stacktrace-log-file=requested.log"][..],
                "refuse --preemption-stacktrace-log-file in the run arguments",
            ),
        ] {
            // Where this host may choose IDs, --no-namespace is admitted and
            // its replays take chosen IDs (`RunOpts::prepare_trial_guest_ids`).
            if run_args == ["--no-namespace"] && crate::app::guest_ids::may_choose_ids().is_ok() {
                continue;
            }
            let error = bisect(run_args);
            assert!(
                error
                    .downcast_ref::<crate::app::container::PolicyRefusal>()
                    .is_some(),
                "{run_args:?}: {error:#}"
            );
            assert!(error.to_string().contains(named), "{run_args:?}: {error:#}");
        }
        // `--no-namespace` is refused above where Hermit cannot choose the
        // guest's IDs: schedule replay, which every bisect trial is, needs IDs
        // that do not change from run to run.
        for admitted in [&["--stacktrace-event=5"][..], &["--timeout=3"][..]] {
            let error = bisect(admitted);
            assert!(
                error.to_string().contains("failed to read --good schedule"),
                "{admitted:?}: {error:#}"
            );
        }

        // A KVM replay with --timeout is refused as `hermit --backend=kvm run
        // --timeout` is (rel-041's review of
        // https://github.com/rrnewton/hermit/pull/3836).
        let argv = [
            "hermit",
            "--backend=kvm",
            "bisect",
            "--good=/nonexistent/good.json",
            "--bad=/nonexistent/bad.json",
            "--",
            "--timeout=3",
            "/bin/true",
        ];
        let args = crate::app::Args::try_parse_from(argv).unwrap();
        let crate::app::Subcommand::Bisect(options) = args.command else {
            panic!("{argv:?} is not bisect")
        };
        let error = options.main(&args.global).unwrap_err();
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
    }

    /// Bisect's replays apply `--skid-margin`: it is recorded as the skid
    /// margin of Reverie's per-process PMU configuration before either
    /// schedule is read, so a later margin is refused. That configuration is
    /// per-process, so the test runs in a process of its own.
    #[test]
    fn bisect_installs_the_replays_skid_margin() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        let argv = [
            "hermit",
            "bisect",
            "--good=/nonexistent/good.json",
            "--bad=/nonexistent/bad.json",
            "--",
            "--skid-margin=4321",
            "/bin/true",
        ];
        let args = crate::app::Args::try_parse_from(argv).unwrap();
        let crate::app::Subcommand::Bisect(options) = args.command else {
            panic!("{argv:?} is not bisect")
        };
        let error = options.main(&args.global).unwrap_err();
        assert!(
            error.to_string().contains("failed to read --good schedule"),
            "{error:#}"
        );
        // A second margin is refused once one is recorded. Neither recording
        // reads the host's CPU, so this holds on a host Reverie has no PMU
        // profile for, such as a GitHub-hosted runner's Emerald Rapids.
        assert!(
            reverie_ptrace::set_skid_margin_override(1).is_err(),
            "bisect did not install the replays' --skid-margin"
        );
    }

    fn known() -> Option<HistoryIdentity> {
        Some(HistoryIdentity::Known(
            reverie::pmu_config::RcbMetric::ConditionalBranchesRetiredUserV1,
        ))
    }

    /// A synthetic schedule standing in for the `fixture` ("passing" or
    /// "failing") endpoint: branch events of threads 3 and 5 in an order of
    /// its own, stating `identity`. It records no run; the checked-in
    /// flaky_cas recordings' producer is unknown, so they are not relabelled.
    fn synthetic_schedule(fixture: &str, identity: Option<HistoryIdentity>) -> serde_json::Value {
        let (three, five) = (detcore::DetTid::from_raw(3), detcore::DetTid::from_raw(5));
        let order = match fixture {
            "passing" => [three, five, three],
            _ => [three, three, five],
        };
        let mut writer = detcore::preemptions::PreemptionWriter::new(None);
        if let Some(identity) = identity {
            writer = writer.with_identity(identity);
        }
        writer.register_thread(three, detcore::DEFAULT_PRIORITY);
        writer.register_thread(five, detcore::DEFAULT_PRIORITY);
        for tid in order {
            writer.insert_schedevent(SchedEvent::branches(tid, 1));
        }
        serde_json::from_str(&writer.into_string()).unwrap()
    }

    /// The settings of a schedule from `synthetic_schedule`: inside its
    /// identity envelope when it has one.
    fn settings(record: &mut serde_json::Value) -> &mut serde_json::Value {
        if record.get("rcb_history").is_some() {
            &mut record["rcb_history"]["recording"]
        } else {
            record
        }
    }

    /// Two synthetic qualified schedules, written to `dir` as the --good and
    /// --bad schedules with the given recorded epochs.
    fn schedules_with_epochs(
        dir: &Path,
        good: Option<&str>,
        bad: Option<&str>,
    ) -> (PathBuf, PathBuf) {
        let write = |fixture: &str, epoch: Option<&str>| {
            let mut record = synthetic_schedule(fixture, known());
            if let Some(epoch) = epoch {
                settings(&mut record)["epoch"] = serde_json::Value::from(epoch);
            }
            let path = dir.join(format!("{fixture}.json"));
            fs::write(&path, record.to_string()).unwrap();
            path
        };
        (write("passing", good), write("failing", bad))
    }

    /// Through `prepare`: the trial configuration `hermit bisect` would replay
    /// these schedules with, or its refusal.
    fn prepared_trials(
        dir: &Path,
        good: Option<&str>,
        bad: Option<&str>,
        run_args: &[&str],
    ) -> Result<AnalyzeOpts, Error> {
        let (good, bad) = schedules_with_epochs(dir, good, bad);
        let good = format!("--good={}", good.display());
        let bad = format!("--bad={}", bad.display());
        let mut argv = vec!["hermit", "bisect", good.as_str(), bad.as_str(), "--"];
        argv.extend(run_args);
        argv.push("/bin/true");
        let args = crate::app::Args::try_parse_from(&argv).unwrap();
        let crate::app::Subcommand::Bisect(options) = args.command else {
            panic!("{argv:?} is not bisect")
        };
        let (mut analyzer, _, _) = options.prepare(&args.global)?;
        analyzer.tmp_dir = Some(dir.to_path_buf());
        Ok(analyzer)
    }

    const RECORDED: &str = "2000-12-31T23:59:59.123456789Z";
    const RECORDED_RFC3339: &str = "2000-12-31T23:59:59.123456789+00:00";

    /// Bisect replays from the epoch its schedules were recorded under, as
    /// `hermit run` does when it replays one
    /// (https://github.com/rrnewton/hermit/issues/3835). Before, every trial
    /// started from the run arguments' epoch, the default when none was
    /// given. The checks run in a child process whose environment fixes
    /// `HERMIT_EPOCH`, which counts as an explicit epoch: once unset, and once
    /// set to an epoch other than the recorded one.
    #[test]
    fn bisect_replays_from_the_epoch_its_schedules_were_recorded_under() {
        const CHILD: &str = "HERMIT_BISECT_EPOCH_TEST_CHILD";
        const OTHER: &str = "2026-01-01T00:00:00Z";
        let Some(arm) = std::env::var_os(CHILD) else {
            let name = format!(
                "{}::bisect_replays_from_the_epoch_its_schedules_were_recorded_under",
                module_path!().split_once("::").unwrap().1
            );
            for arm in ["unset", "set"] {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child
                    .args(["--exact", &name, "--nocapture"])
                    .env(CHILD, arm);
                if arm == "set" {
                    child.env("HERMIT_EPOCH", OTHER);
                } else {
                    child.env_remove("HERMIT_EPOCH");
                }
                let output = child.output().unwrap();
                assert!(output.status.success(), "{arm}: {output:?}");
                assert!(
                    String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                    "{arm}: the child ran no test: {output:?}"
                );
            }
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        if arm == "set" {
            let error = prepared_trials(dir, Some(RECORDED), Some(RECORDED), &[]).unwrap_err();
            assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
            assert!(
                error.to_string().contains(&format!(
                    "the explicit virtual-time epoch 2026-01-01T00:00:00+00:00 (from --epoch or \
                     HERMIT_EPOCH) differs from the epoch {RECORDED_RFC3339}"
                )),
                "{error:#}"
            );
            return;
        }

        // The default epoch, for schedules that carry none.
        let trials = prepared_trials(dir, None, None, &[]).unwrap();
        // No `--epoch` is added; `--expired-wait-charge` and
        // `--sleep-registration` always are.
        assert_eq!(
            trials.run_arg,
            [
                "--expired-wait-charge=each",
                "--sleep-registration=when-chosen"
            ]
        );
        assert_eq!(trials.trial_epoch_for_test(), "2026-01-01T00:00:00+00:00");

        // The recorded epoch, from either schedule or both.
        for (good, bad) in [
            (Some(RECORDED), Some(RECORDED)),
            (Some(RECORDED), None),
            (None, Some(RECORDED)),
        ] {
            let trials = prepared_trials(dir, good, bad, &[]).unwrap();
            assert_eq!(
                trials.trial_epoch_for_test(),
                RECORDED_RFC3339,
                "{good:?} / {bad:?}"
            );
        }

        // An explicit epoch equal to the recorded one adds no second --epoch.
        let trials =
            prepared_trials(dir, Some(RECORDED), Some(RECORDED), &["--epoch", RECORDED]).unwrap();
        // No `--epoch` is added; `--expired-wait-charge` and
        // `--sleep-registration` always are.
        assert_eq!(
            trials.run_arg,
            [
                "--expired-wait-charge=each",
                "--sleep-registration=when-chosen"
            ]
        );
        assert_eq!(trials.trial_epoch_for_test(), RECORDED_RFC3339);

        // An explicit epoch that differs is refused, naming the remedy.
        let error =
            prepared_trials(dir, Some(RECORDED), Some(RECORDED), &["--epoch", OTHER]).unwrap_err();
        assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
        assert!(
            error
                .to_string()
                .contains(&format!("pass --epoch={RECORDED_RFC3339}")),
            "{error:#}"
        );
    }

    /// Schedules recorded under two different epochs are refused before any
    /// replay: no single replay epoch reproduces both.
    #[test]
    fn bisect_refuses_schedules_recorded_under_two_epochs() {
        let dir = tempfile::tempdir().unwrap();
        let error = prepared_trials(
            dir.path(),
            Some(RECORDED),
            Some("2026-01-01T00:00:00Z"),
            &[],
        )
        .unwrap_err();
        assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
        let message = error.to_string();
        assert!(
            message.contains(RECORDED_RFC3339) && message.contains("2026-01-01T00:00:00+00:00"),
            "{error:#}"
        );
    }

    /// Bisect replays every trial under the `--expired-wait-charge` of the
    /// `--bad` schedule (unless its run arguments give one), and refuses a
    /// `--good` schedule recorded under the other rule.
    #[test]
    fn bisect_replays_under_the_bad_schedules_expired_wait_charge() {
        use detcore_model::config::ExpiredWaitCharge;
        let dir = tempfile::tempdir().unwrap();
        let write = |fixture: &str, name: &str, once: bool| {
            let mut record = synthetic_schedule(fixture, known());
            if once {
                settings(&mut record)["expired_wait_runs_charged_once"] =
                    serde_json::Value::from(true);
            }
            let path = dir.path().join(name);
            fs::write(&path, record.to_string()).unwrap();
            path
        };
        let prepare = |good: &Path, bad: &Path, run_args: &[&str]| {
            let good = format!("--good={}", good.display());
            let bad = format!("--bad={}", bad.display());
            let mut argv = vec!["hermit", "bisect", good.as_str(), bad.as_str(), "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = super::super::Args::try_parse_from(&argv).unwrap();
            let super::super::Subcommand::Bisect(options) = args.command else {
                panic!("{argv:?} is not bisect")
            };
            options
                .prepare(&args.global)
                .map(|(analyzer, _, _)| analyzer.trial_expired_wait_charge())
        };
        let good_each = write("passing", "good-each.json", false);
        let bad_each = write("failing", "bad-each.json", false);
        let good_once = write("passing", "good-once.json", true);
        let bad_once = write("failing", "bad-once.json", true);
        assert_eq!(
            prepare(&good_each, &bad_each, &[]).unwrap(),
            ExpiredWaitCharge::Each
        );
        assert_eq!(
            prepare(&good_once, &bad_once, &[]).unwrap(),
            ExpiredWaitCharge::Once
        );
        for (good, bad, run_args) in [
            (&good_each, &bad_once, vec![]),
            (&good_once, &bad_each, vec![]),
            (&good_each, &bad_each, vec!["--expired-wait-charge=once"]),
        ] {
            let error = prepare(good, bad, &run_args).unwrap_err();
            assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
            assert!(
                format!("{error:#}").contains("--expired-wait-charge"),
                "{error:#}"
            );
        }
    }

    /// Bisect refuses, before any trial, a `--good` or `--bad` schedule made
    /// under another `--pipe-wake-latency` than the trials' own
    /// (https://github.com/rrnewton/hermit/issues/4380), and any schedule in
    /// its run arguments.
    /// Bisect replays every trial under the `--sleep-registration` of the
    /// `--bad` schedule (unless its run arguments give one), and refuses a
    /// `--good` schedule recorded under the other rule.
    #[test]
    fn bisect_replays_under_the_bad_schedules_sleep_registration() {
        use detcore_model::config::SleepRegistration;
        let dir = tempfile::tempdir().unwrap();
        let write = |fixture: &str, name: &str, first_pass: bool| {
            let mut record = synthetic_schedule(fixture, known());
            if first_pass {
                settings(&mut record)["queued_sleeps_registered_at_first_pass"] =
                    serde_json::Value::from(true);
            }
            let path = dir.path().join(name);
            fs::write(&path, record.to_string()).unwrap();
            path
        };
        let prepare = |good: &Path, bad: &Path, run_args: &[&str]| {
            let good = format!("--good={}", good.display());
            let bad = format!("--bad={}", bad.display());
            let mut argv = vec!["hermit", "bisect", good.as_str(), bad.as_str(), "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = crate::app::Args::try_parse_from(&argv).unwrap();
            let crate::app::Subcommand::Bisect(options) = args.command else {
                panic!("{argv:?} is not bisect")
            };
            options
                .prepare(&args.global)
                .map(|(analyzer, _, _)| analyzer.trial_sleep_registration())
        };
        let good_chosen = write("passing", "good-chosen.json", false);
        let bad_chosen = write("failing", "bad-chosen.json", false);
        let good_first = write("passing", "good-first.json", true);
        let bad_first = write("failing", "bad-first.json", true);
        assert_eq!(
            prepare(&good_chosen, &bad_chosen, &[]).unwrap(),
            SleepRegistration::WhenChosen
        );
        assert_eq!(
            prepare(&good_first, &bad_first, &[]).unwrap(),
            SleepRegistration::FirstPass
        );
        for (good, bad, run_args) in [
            (&good_chosen, &bad_first, vec![]),
            (&good_first, &bad_chosen, vec![]),
            (
                &good_chosen,
                &bad_chosen,
                vec!["--sleep-registration=first-pass"],
            ),
        ] {
            let error = prepare(good, bad, &run_args).unwrap_err();
            assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
            assert!(
                format!("{error:#}").contains("--sleep-registration"),
                "{error:#}"
            );
        }
    }

    /// Bisect refuses, before any trial, a `--good` or `--bad` schedule, or a
    /// record its run arguments replay, made under another
    /// `--pipe-wake-latency` than the trials' own
    /// (https://github.com/rrnewton/hermit/issues/4380).
    #[test]
    fn bisect_refuses_schedules_under_another_pipe_wake_latency() {
        let dir = tempfile::tempdir().unwrap();
        let write = |fixture: &str, name: &str, latency: Option<u64>| {
            let mut record = synthetic_schedule(fixture, known());
            if let Some(latency) = latency {
                settings(&mut record)["pipe_wake_latency"] = serde_json::Value::from(latency);
            }
            let path = dir.path().join(name);
            fs::write(&path, record.to_string()).unwrap();
            path
        };
        let prepare = |good: &Path, bad: &Path, run_args: &[&str]| {
            let good = format!("--good={}", good.display());
            let bad = format!("--bad={}", bad.display());
            let mut argv = vec!["hermit", "bisect", good.as_str(), bad.as_str(), "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = crate::app::Args::try_parse_from(&argv).unwrap();
            let crate::app::Subcommand::Bisect(options) = args.command else {
                panic!("{argv:?} is not bisect")
            };
            options.prepare(&args.global).map(|_| ())
        };
        let good_off = write("passing", "good-off.json", None);
        let bad_off = write("failing", "bad-off.json", None);
        let good_zero = write("passing", "good-zero.json", Some(0));
        let good_three = write("passing", "good-three.json", Some(3_000_000));
        let bad_three = write("failing", "bad-three.json", Some(3_000_000));
        let replayed_zero = write("passing", "replayed-zero.json", Some(0));
        let replay_flag = format!("--replay-schedule-from={}", replayed_zero.display());

        assert!(prepare(&good_off, &bad_off, &[]).is_ok());
        assert!(prepare(&good_three, &bad_three, &["--pipe-wake-latency=3000000"]).is_ok());
        for (good, bad, run_args, named) in [
            (&good_zero, &bad_off, vec![], "--good"),
            (&good_off, &bad_off, vec!["--pipe-wake-latency=0"], "--good"),
            (
                &good_three,
                &bad_three,
                vec!["--pipe-wake-latency=0"],
                "--good",
            ),
        ] {
            let error = prepare(good, bad, &run_args).unwrap_err();
            assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
            let message = format!("{error:#}");
            assert!(
                message.contains(named) && message.contains("cannot reproduce the run"),
                "{run_args:?}: {message}"
            );
        }
        // A schedule in the run arguments is refused before any recording is
        // read, whatever its latency: no run bisect launches would replay it
        // (`RunOpts::options_trials_refuse`).
        // A missing `--good` shows that no schedule was read before it.
        for good in [good_off.clone(), dir.path().join("missing.json")] {
            let error = prepare(&good, &bad_off, &[replay_flag.as_str()]).unwrap_err();
            assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
            assert!(
                format!("{error:#}").contains("refuse --replay-schedule-from in the run arguments"),
                "{error:#}"
            );
        }
    }

    /// Bisect refuses, before any trial, a pair of schedules whose RCB
    /// coordinates it cannot execute or mix: an unqualified or legacy
    /// endpoint, or endpoints stating different identities. On any host one
    /// of the two refusals applies, so no trial schedule is ever written.
    #[test]
    fn bisect_refuses_endpoints_it_cannot_execute_or_mix() {
        let dir = tempfile::tempdir().unwrap();
        let write = |fixture: &str, name: &str, identity: Option<HistoryIdentity>| {
            let path = dir.path().join(name);
            fs::write(&path, synthetic_schedule(fixture, identity).to_string()).unwrap();
            path
        };
        let unqualified = || Some(HistoryIdentity::Unqualified("claim-unset".to_owned()));
        for (good, bad) in [
            (known(), unqualified()),
            (unqualified(), known()),
            (known(), None),
        ] {
            let good = write("passing", "good.json", good);
            let bad = write("failing", "bad.json", bad);
            let good = format!("--good={}", good.display());
            let bad = format!("--bad={}", bad.display());
            let argv = [
                "hermit",
                "bisect",
                good.as_str(),
                bad.as_str(),
                "--",
                "/bin/true",
            ];
            let args = crate::app::Args::try_parse_from(argv).unwrap();
            let crate::app::Subcommand::Bisect(options) = args.command else {
                panic!("{argv:?} is not bisect")
            };
            let error = options.prepare(&args.global).map(|_| ()).unwrap_err();
            assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
            assert!(format!("{error:#}").contains("hermit refused"), "{error:#}");
        }
    }

    #[cfg_attr(
        fbcode_build,
        ignore = "fbsource-disabled: compile-time CARGO_MANIFEST_DIR is not present at test runtime"
    )]
    #[test]
    fn schedule_fixture_contains_events() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        let schedule = read_schedule(
            &Path::new(&manifest_dir)
                .join("test-resources/flaky_cas_sequence_schedules-passing.json"),
            "good",
        )
        .unwrap();
        assert!(schedule.contains_schedevents());
    }
}

#[cfg(test)]
mod replay_departure_tests {
    use detcore::types::SchedEvent;

    use super::*;

    #[test]
    fn bisection_refuses_a_marked_schedule_before_event_projection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("schedule.json");
        for marker in [None, Some(7)] {
            let record = PreemptionRecord::from_sched_events(vec![SchedEvent::branches(
                detcore::types::DetTid::from_raw(3),
                1,
            )])
            .retain_replay_departure(marker);
            record.write_to_disk(&path).unwrap();
            let result = read_schedule(&path, "good");
            if marker.is_some() {
                let error = result.expect_err("bisection must not drop departure provenance");
                assert!(
                    error
                        .downcast_ref::<crate::app::container::PolicyRefusal>()
                        .is_some(),
                    "{error:#}"
                );
                assert!(format!("{error:#}").contains("HERMIT_INCOMPLETE_REPLAY_RECORD"));
            } else {
                assert_eq!(result.unwrap(), record);
            }
        }
    }
}
