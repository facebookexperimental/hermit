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
use detcore::preemptions::PreemptionRecord;
use detcore::types::SchedEvent;
use hermit::Error;
use regex::Regex;
use reverie::process::ExitStatus;
use tracing::metadata::LevelFilter;

use super::analyze::AnalyzeOpts;
use super::analyze::ExitStatusConstraint;
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

    /// Exit status identifying the target failure.
    #[clap(long, default_value = "nonzero", value_name = "NUM|nonzero|any")]
    target_exit_code: ExitStatusConstraint,

    /// Logging level for replayed guest runs.
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

    /// Arguments for the underlying `hermit run`, followed by the program and its arguments.
    #[clap(value_name = "RUN_ARGS", required = true)]
    run_args: Vec<String>,

    /// Why this host's retired-branch counter is inexact, if it is (see
    /// `AnalyzeOpts::inexact_branch_counter`).
    #[clap(skip = crate::host_capabilities::host_inexact_branch_counter as fn() -> Option<String>)]
    inexact_branch_counter: fn() -> Option<String>,
}

impl BisectOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let (mut analyzer, good, bad) = self.prepare(global)?;
        analyzer.bisect_schedule_pair(good, bad)
    }

    /// Everything `main` does before the first replay: the refusals, the two
    /// schedules, and the trial configuration they imply.
    fn prepare(
        &self,
        global: &GlobalOpts,
    ) -> Result<(AnalyzeOpts, Vec<SchedEvent>, Vec<SchedEvent>), Error> {
        let mut analyzer = AnalyzeOpts {
            target_stdout: self.target_stdout.clone(),
            target_stderr: self.target_stderr.clone(),
            target_exit_code: self.target_exit_code.clone(),
            guest_log: self.guest_log,
            selfcheck: false,
            search: false,
            run_needleman: self.needleman,
            minimize: false,
            imprecise_search: false,
            run1_seed: None,
            run1_preemptions: None,
            run1_schedule: None,
            run2_seed: None,
            run2_preemptions: None,
            run2_schedule: None,
            report_file: self.report_file.clone(),
            analyze_seed: None,
            verbose: self.verbose,
            jitter_dist: self.jitter_dist,
            execution_context: self.execution_context,
            tmp_dir: None,
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
        };
        // Before the schedules are read: a refused cap or counter reads and
        // starts nothing.
        analyzer.refuse_unsupervised_log_cap()?;
        analyzer.refuse_options_trials_do_not_apply()?;
        analyzer.refuse_outputs_trials_overwrite()?;
        analyzer.refuse_unqualified_trial_timeout()?;
        analyzer.install_trial_pmu_config()?;
        analyzer.admit_branch_counter(&mut crate::tracing::write_stderr_diagnostic)?;

        let good = read_schedule(&self.good, "good")?;
        let bad = read_schedule(&self.bad, "bad")?;
        // Each trial's schedule is written anew without an epoch, so the
        // trials replay from the one the two schedules were recorded under.
        analyzer.adopt_recorded_epoch(&[("--good", &self.good), ("--bad", &self.bad)])?;
        // Every replay charges expired timed waits as the failing schedule did.
        analyzer.adopt_expired_wait_charge(Some(&self.bad))?;
        analyzer.refuse_recordings_under_another_pipe_wake_latency(&[
            ("--good", &self.good),
            ("--bad", &self.bad),
        ])?;
        let mut good = good.into_global();
        let mut bad = bad.into_global();
        // Analyze's trial outputs omit optional event timestamps. Put these
        // admitted search inputs in that same representation: bisection edits
        // event order, not the original recordings' virtual-time trajectories.
        for event in good.iter_mut().chain(&mut bad) {
            event.end_time = None;
        }
        if good == bad {
            bail!("the --good and --bad schedules contain identical event traces");
        }
        Ok((analyzer, good, bad))
    }
}

fn read_schedule(path: &Path, label: &str) -> anyhow::Result<PreemptionRecord> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read --{label} schedule {}", path.display()))?;
    let record: PreemptionRecord = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse --{label} schedule {}", path.display()))?;
    record
        .validate()
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("--{label} schedule {} failed validation", path.display()))?;
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
    use std::num::NonZeroUsize;

    use detcore::types::DetTid;
    use detcore::types::LogicalTime;
    use detcore::types::Op;
    use detcore::types::SyscallPhase;
    use reverie::syscalls::Sysno;

    use super::*;
    use crate::container::PolicyRefusal;

    fn search_record(events: Vec<SchedEvent>) -> serde_json::Value {
        let record = PreemptionRecord::from_sched_events(events)
            .with_pipe_wake_latency(Some(3_000_000))
            .with_signal_state_copy(Some(true));
        let mut record = serde_json::to_value(record).unwrap();
        record["epoch"] = serde_json::json!(RECORDED);
        record
    }

    /// Exercise production admission and conversion without launching a trial;
    /// both successful preparation and refusal must leave the inputs intact.
    fn prepare_search_records(
        good: &serde_json::Value,
        bad: &serde_json::Value,
    ) -> Result<(AnalyzeOpts, Vec<SchedEvent>, Vec<SchedEvent>), Error> {
        let dir = tempfile::tempdir().unwrap();
        let good_path = dir.path().join("good.json");
        let bad_path = dir.path().join("bad.json");
        let good_bytes = serde_json::to_vec(good).unwrap();
        let bad_bytes = serde_json::to_vec(bad).unwrap();
        fs::write(&good_path, &good_bytes).unwrap();
        fs::write(&bad_path, &bad_bytes).unwrap();
        let args = crate::Args::try_parse_from([
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
        let crate::Subcommand::Bisect(options) = args.command else {
            panic!("expected bisect")
        };
        let result = options.prepare(&args.global);
        assert_eq!(fs::read(good_path).unwrap(), good_bytes);
        assert_eq!(fs::read(bad_path).unwrap(), bad_bytes);
        result
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
            assert_eq!(prepared_good, good, "{field}: good input");
            assert_eq!(prepared_bad, bad, "{field}: bad input");
            assert_ne!(prepared_good, prepared_bad, "{field}");
            trials.tmp_dir = Some(trials_dir.path().to_path_buf());
            assert_eq!(trials.trial_epoch_for_test(), RECORDED_RFC3339);
            assert_eq!(trials.trial_pipe_wake_latency(), Some(3_000_000));
            assert!(trials.trial_signal_state_copy());
        }
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
        ] {
            for change_good in [true, false] {
                let mut good = good.clone();
                let mut bad = bad.clone();
                let changed = if change_good { &mut good } else { &mut bad };
                changed[field] = value.clone();
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
        let args = crate::Args::try_parse_from(argv).unwrap();
        let crate::Subcommand::Bisect(mut options) = args.command else {
            panic!("{argv:?} is not bisect")
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
            let args = crate::Args::try_parse_from(&argv).unwrap();
            let crate::Subcommand::Bisect(options) = args.command else {
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
            let error = bisect(run_args);
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{run_args:?}: {error:#}"
            );
            assert!(error.to_string().contains(named), "{run_args:?}: {error:#}");
        }
        // `--no-namespace` is refused above: schedule replay, which every
        // bisect trial is, needs stable namespace PIDs.
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
        let args = crate::Args::try_parse_from(argv).unwrap();
        let crate::Subcommand::Bisect(options) = args.command else {
            panic!("{argv:?} is not bisect")
        };
        let error = options.main(&args.global).unwrap_err();
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
    }

    /// Bisect's replays apply `--skid-margin`: it is recorded as the skid
    /// margin of Reverie's per-process PMU configuration before either
    /// schedule is read, so a later margin is refused. This relies on nextest running each
    /// test in a process of its own, as the validation DAG does.
    #[test]
    fn bisect_installs_the_replays_skid_margin() {
        let argv = [
            "hermit",
            "bisect",
            "--good=/nonexistent/good.json",
            "--bad=/nonexistent/bad.json",
            "--",
            "--skid-margin=4321",
            "/bin/true",
        ];
        let args = crate::Args::try_parse_from(argv).unwrap();
        let crate::Subcommand::Bisect(options) = args.command else {
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

    /// The two flaky_cas fixtures, written to `dir` as the --good and --bad
    /// schedules with the given recorded epochs.
    fn schedules_with_epochs(
        dir: &Path,
        good: Option<&str>,
        bad: Option<&str>,
    ) -> (PathBuf, PathBuf) {
        let resources = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-resources");
        let write = |fixture: &str, epoch: Option<&str>| {
            let source = resources.join(format!("flaky_cas_sequence_schedules-{fixture}.json"));
            let mut record: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(source).unwrap()).unwrap();
            if let Some(epoch) = epoch {
                record["epoch"] = serde_json::Value::from(epoch);
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
        let args = crate::Args::try_parse_from(&argv).unwrap();
        let crate::Subcommand::Bisect(options) = args.command else {
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
        // No `--epoch` is added; `--expired-wait-charge` always is.
        assert_eq!(trials.run_arg, ["--expired-wait-charge=each"]);
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
        // No `--epoch` is added; `--expired-wait-charge` always is.
        assert_eq!(trials.run_arg, ["--expired-wait-charge=each"]);
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
        let resources = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-resources");
        let dir = tempfile::tempdir().unwrap();
        let write = |fixture: &str, name: &str, once: bool| {
            let source = resources.join(format!("flaky_cas_sequence_schedules-{fixture}.json"));
            let mut record: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(source).unwrap()).unwrap();
            if once {
                record["expired_wait_runs_charged_once"] = serde_json::Value::from(true);
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
            let args = crate::Args::try_parse_from(&argv).unwrap();
            let crate::Subcommand::Bisect(options) = args.command else {
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

    /// Bisect refuses, before any trial, a `--good` or `--bad` schedule, or a
    /// record its run arguments replay, made under another
    /// `--pipe-wake-latency` than the trials' own
    /// (https://github.com/rrnewton/hermit/issues/4380).
    #[test]
    fn bisect_refuses_schedules_under_another_pipe_wake_latency() {
        let resources = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-resources");
        let dir = tempfile::tempdir().unwrap();
        let write = |fixture: &str, name: &str, latency: Option<u64>| {
            let source = resources.join(format!("flaky_cas_sequence_schedules-{fixture}.json"));
            let mut record: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(source).unwrap()).unwrap();
            if let Some(latency) = latency {
                record["pipe_wake_latency"] = serde_json::Value::from(latency);
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
            let args = crate::Args::try_parse_from(&argv).unwrap();
            let crate::Subcommand::Bisect(options) = args.command else {
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
            (
                &good_off,
                &bad_off,
                vec![replay_flag.as_str()],
                "--replay-schedule-from",
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
    }

    #[test]
    fn schedule_fixture_contains_events() {
        let schedule = read_schedule(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test-resources/flaky_cas_sequence_schedules-passing.json"),
            "good",
        )
        .unwrap();
        assert!(schedule.contains_schedevents());
    }
}
