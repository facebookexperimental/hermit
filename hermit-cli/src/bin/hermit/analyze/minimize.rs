/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A mode for analyzing a hermit run.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use colored::Colorize;
use detcore::DetTid;
use detcore::FIRST_PRIORITY;
use detcore::LAST_PRIORITY;
use detcore::Priority;
use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use detcore::types::LogicalTime;
use detcore::util::truncated;
use rand::RngExt as _;
use rand::SeedableRng;
use rand_pcg::Pcg64Mcg;

use crate::app::analyze::types::AnalyzeOpts;
use crate::app::global_opts::GlobalOpts;

/// Sanity check that a series of preemptions don't include duplicates and are monotonically increasing.
fn sanity_preempts(vec: &[(LogicalTime, Priority)]) -> anyhow::Result<()> {
    let mut set = BTreeSet::new();

    let mut time_last = None;
    for (ns, prio) in vec {
        if let Some(last) = time_last
            && *ns <= last
        {
            bail!(
                "Timestamps failed to monotonically increase ({}), in series:\n {:?}",
                ns,
                vec
            );
        }
        assert!(*prio >= FIRST_PRIORITY);
        assert!(*prio <= LAST_PRIORITY);
        time_last = Some(*ns);
        if !set.insert(ns) {
            bail!(
                "sanity_preempts: time series of preemptions contained duplicate entries sharing the timestamp {}",
                ns
            );
        }
    }
    Ok(())
}

impl AnalyzeOpts {
    /// Iteratively minimize the schedule needed to produce the error.
    ///
    /// # Returns
    /// - Minimized preemption record (in memory),
    /// - Path of a file containing that same minimized preemption record,
    /// - Path of the log file that corresponds to the last matching (minimal) run.
    pub(super) fn minimize(
        &self,
        preempts_path: &Path,
        _global: &GlobalOpts,
    ) -> anyhow::Result<(PreemptionRecord, PathBuf, PathBuf)> {
        self.minimize_with(preempts_path, |runname, new_preempts_path| {
            Ok(self
                .launch_from_preempts_to_sched(runname, new_preempts_path, None)?
                .0)
        })
    }

    /// The minimization loop behind [`Self::minimize`]. `trial_matches` runs
    /// one trial with the preemptions in the given file and says whether it
    /// met the criteria. A trial that cannot finish (for example, one that
    /// `--max-log-bytes` ended) ends the minimization with its error
    /// unchanged, so main still reports it by its class.
    fn minimize_with(
        &self,
        preempts_path: &Path,
        mut trial_matches: impl FnMut(&str, &Path) -> anyhow::Result<bool>,
    ) -> anyhow::Result<(PreemptionRecord, PathBuf, PathBuf)> {
        let pr = PreemptionReader::new(preempts_path);
        let mut tmp_dir = preempts_path.to_path_buf();
        assert!(tmp_dir.pop());

        let tids: Vec<DetTid> = pr.all_threads();
        let init_schedule: PreemptionRecord = pr.load_all();
        super::rundata::RunData::require_complete_record(&init_schedule, preempts_path)?;
        // An unchanged endpoint is still a transformation input: an old
        // Chosen record must not acquire provenance through minimization.
        init_schedule.validate_combined_reserved_inputs(&init_schedule)?;
        let mut round: u64 = 0;

        // All the preemption points we don't know about yet.  Are they critical?
        let mut remaining_unknown: BTreeMap<DetTid, _> = BTreeMap::new();
        let mut init_intervention_count = 0;
        for (dtid, history) in init_schedule.extract_all() {
            let vec = history.as_vec();
            sanity_preempts(&vec)?;
            init_intervention_count += vec.len();
            remaining_unknown.insert(dtid, vec);
        }

        eprintln!(
            ":: {}",
            format!(
                "RootCause: starting with schedule of {} interventions for {} threads.",
                init_intervention_count,
                tids.len()
            )
            .yellow()
            .bold()
        );
        if self.verbose {
            eprintln!(
                "Initial schedule:\n{}",
                truncated(1000, format!("{}", init_schedule))
            );
        }

        // The ones we know for sure are critical.
        let mut critical_preempts: BTreeMap<DetTid, BTreeSet<(LogicalTime, Priority)>> =
            BTreeMap::new();

        // We take threads out of consideration if they have no interventions:
        let all_threads = pr.all_threads();
        let mut remaining_threads = Vec::new();

        let min_seed = self.analyze_seed.unwrap_or_else(rand::random::<u64>);
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
                let sum: usize = remaining_unknown.values().map(|v| v.len()).sum();
                let sum2: usize = critical_preempts.values().map(|s| s.len()).sum();
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
                                .yellow()
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

            let selected_ix = rng.random_range(0..remaining_threads.len());
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
                    btmap
                        .iter_mut()
                        .try_for_each(|(tid, vec)| -> anyhow::Result<()> {
                            if let Some(set) = critical_preempts.get(tid) {
                                for preempt in set {
                                    vec.push(*preempt);
                                }
                                // if cfg!(debug)
                                {
                                    sanity_preempts(vec)?;
                                }
                            }
                            Ok(())
                        })?;
                    // A record with instruction offsets keeps each kept
                    // row's branch target and offset (`from_vecs_of`).
                    self.stamp_trial_record(
                        PreemptionRecord::from_vecs_of(&btmap, &init_schedule)
                            .map_err(anyhow::Error::msg)?
                            .with_guest_ids(init_schedule.guest_ids().unwrap_or_default())
                            .with_reserved_input(init_schedule.reserved_input().clone()),
                    )
                };
                if let Err(e) = pr_new.validate() {
                    bail!(
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
                    .map_err(anyhow::Error::msg)?;
                last_attempt = Some(pr_new);

                let batch = batch_sizes.get_mut(&selected_tid).unwrap();
                let runname = format!("round_{:0wide$}", round, wide = 3);
                let log_path = tmp_dir.join(&runname).with_extension("log");
                if trial_matches(&runname, &new_preempts_path)? {
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
                        let entry = critical_preempts.entry(selected_tid).or_default();

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
                        // A cut of the thread's last untested interventions
                        // took it out of consideration; they are untested
                        // again, so it is back in.
                        if !remaining_threads.contains(&selected_tid) {
                            remaining_threads.push(selected_tid);
                        }
                        continue;
                    }
                }
            }
        }
        Ok((
            last_matching_attempt.expect("at least one run to match the criteria"),
            last_matching_pr_file.expect("at least one run to match the criteria"),
            last_matching_log.expect("at least one run to match the criteria"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;
    use crate::app::container::LogCapExceeded;

    /// Minimizing a record with `--chaos-instruction-offset` decisions keeps
    /// each kept row's exact branch target and offset in every trial, so a
    /// trial still stops where the record did, and the minimal record keeps
    /// the critical row's offset, in the rows-only form a preemption replay
    /// arms (no format: rows alone claim no schedule trace and no jumps).
    /// Here the target outcome needs exactly the row that ended past its
    /// branch target.
    #[test]
    fn minimizing_keeps_each_rows_branch_target_and_instruction_offset() {
        use detcore::preemptions::InstructionOffset;
        use detcore::preemptions::PreemptionWriter;
        let workspace = tempfile::tempdir().unwrap();
        let preempts_path = workspace.path().join("target.preempts");
        let tid = DetTid::from_raw(3);
        let critical = InstructionOffset {
            rcb_target: 5_000,
            instructions: 6,
        };
        let mut writer = PreemptionWriter::new(None);
        writer.register_thread(tid, detcore::DEFAULT_PRIORITY);
        let mut prior = detcore::DEFAULT_PRIORITY;
        for k in 1..=8u64 {
            let next = FIRST_PRIORITY + k;
            writer.insert_reprioritization(
                tid,
                LogicalTime::from_nanos(k * 1000),
                k * 1000 + 2,
                prior,
                next,
                (k == 5).then_some(critical),
            );
            prior = next;
        }
        std::fs::write(&preempts_path, writer.into_string()).unwrap();
        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--analyze-seed=7", "--", "/bin/true"])
                .unwrap();
        let mut trials = 0;
        let (minimal, _, _) = options
            .minimize_with(&preempts_path, |_runname, trial_preempts| {
                trials += 1;
                let trial = PreemptionReader::new(trial_preempts).load_all();
                let history = trial.extract_all().remove(&tid).unwrap();
                let mut rows = history.into_iter();
                let mut has_critical = false;
                while let Some((time, _, rcbs, offset)) = rows.next_with_offset() {
                    // Every kept row ends at its recorded branch count.
                    assert_eq!(rcbs, Some(time.as_nanos() + 2), "trial {trials}");
                    has_critical |= offset == Some(critical);
                    assert!(offset.is_none() || offset == Some(critical));
                }
                assert_eq!(
                    trial.carries_instruction_offsets(),
                    has_critical,
                    "trial {trials}"
                );
                Ok(has_critical)
            })
            .unwrap();
        assert!(trials > 1);
        assert_eq!(
            minimal.format_version(),
            Some(detcore::preemptions::INSTRUCTION_OFFSET_ROWS_FORMAT)
        );
        assert_eq!(minimal.timing_format(), None);
        assert_eq!(
            minimal.recorded_offsets(),
            detcore::preemptions::RecordedOffsets::RowsOnly
        );
        // The minimal record keeps the critical row, at its exact branch
        // target and offset, and drops every row after it.
        let mut rows = minimal.extract_all().remove(&tid).unwrap().into_iter();
        let mut kept = Vec::new();
        while let Some((time, _, rcbs, offset)) = rows.next_with_offset() {
            kept.push((time.as_nanos(), rcbs, offset));
        }
        assert_eq!(
            kept.last(),
            Some(&(5_000, Some(5_002), Some(critical))),
            "{kept:?}"
        );
        assert!(kept.len() < 8, "{kept:?}");
    }

    /// Every record minimization writes, its trials' and the minimal one,
    /// holds rows alone and claims no timing format, from a source with jumps
    /// of global time with or without instruction offsets: format 2 and every
    /// later one claim a complete list of jumps, which rows alone do not hold.
    /// One whose rows keep an offset states format 6
    /// (`INSTRUCTION_OFFSET_ROWS_FORMAT`), which a Hermit that predates the
    /// offsets (formats 1 to 5) refuses by name rather than replaying the
    /// rows without them, and whose rows-only timing is format 1's
    /// (`PreemptionRecord::timing_format`); one without states none, as
    /// before. Its rows keep their exact branch targets and offsets. Each written
    /// record validates and reads back, and `hermit run`'s replay guard lets
    /// the preemption replay that runs it through, while a schedule replay of
    /// one that keeps an offset is refused with the rows-only message, as it
    /// has no schedule trace to arm the offset at. Analyze's input check
    /// accepts both sources, and refuses a minimal record that keeps an offset
    /// when it is given back to analyze as an input, as any rows-only record.
    #[test]
    fn a_minimize_record_states_only_what_its_rows_hold() {
        use detcore::preemptions::INSTRUCTION_OFFSET_ROWS_FORMAT;
        use detcore::preemptions::InstructionOffset;
        use detcore::preemptions::PreemptionWriter;
        use detcore::preemptions::RecordedOffsets;
        use detcore::preemptions::TimeSkip;
        use detcore::preemptions::TimeSkipReason;
        use detcore::types::SchedEvent;
        let tid = DetTid::from_raw(3);
        let critical = InstructionOffset {
            rcb_target: 5_000,
            instructions: 6,
        };
        let jumps = vec![
            TimeSkip {
                event: 1,
                target: LogicalTime::from_nanos(50_000),
                reason: TimeSkipReason::IdleGuestPollers,
            },
            TimeSkip {
                event: 2,
                target: LogicalTime::from_nanos(90_000),
                reason: TimeSkipReason::IdleGuestPollers,
            },
        ];
        for with_offsets in [false, true] {
            let workspace = tempfile::tempdir().unwrap();
            let preempts_path = workspace.path().join("target.preempts");
            let mut writer = PreemptionWriter::new(None)
                .with_format(Some(detcore::preemptions::PREEMPTION_RECORD_FORMAT_MAX));
            writer.register_thread(tid, detcore::DEFAULT_PRIORITY);
            let mut prior = detcore::DEFAULT_PRIORITY;
            for k in 1..=8u64 {
                let next = FIRST_PRIORITY + k;
                writer.insert_reprioritization(
                    tid,
                    LogicalTime::from_nanos(k * 1000),
                    k * 1000 + 2,
                    prior,
                    next,
                    (with_offsets && k == 5).then_some(critical),
                );
                prior = next;
            }
            writer.insert_schedevent(
                SchedEvent::branches(tid, 5_002)
                    .with_instruction_offset(with_offsets.then_some(critical)),
            );
            writer.insert_schedevent(SchedEvent::branches(tid, 3));
            let source: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
            let source = source.with_time_skips(jumps.clone());
            source.validate().unwrap();
            assert_eq!(
                source.format_version(),
                Some(if with_offsets {
                    INSTRUCTION_OFFSET_ROWS_FORMAT
                } else {
                    // A writer that notes no background starts states the
                    // format below theirs.
                    detcore::preemptions::BACKGROUND_STARTS_FORMAT - 1
                })
            );
            source.write_to_disk(&preempts_path).unwrap();
            // The exact row of the source each kept row must match, by the time
            // it ended: from a source with offsets its branch target and
            // offset (one without is rebuilt by time alone, as `from_vecs`
            // rebuilds it).
            let source_rows: BTreeMap<LogicalTime, (Option<u64>, Option<InstructionOffset>)> = {
                let mut rows = source.extract_all().remove(&tid).unwrap().into_iter();
                let mut map = BTreeMap::new();
                while let Some((time, _, rcbs, offset)) = rows.next_with_offset() {
                    map.insert(time, (rcbs.filter(|_| with_offsets), offset));
                }
                map
            };

            let options =
                AnalyzeOpts::try_parse_from(["analyze", "--analyze-seed=7", "--", "/bin/true"])
                    .unwrap();
            let trial = crate::app::analyze::rundata::RunData::raw_runopts(&options);
            let mut schedule_replay = trial.det_opts.det_config.clone();
            schedule_replay.chaos = false;
            let guard = |recorded, schedule| {
                crate::app::run::instruction_offset_replay_refusal(
                    recorded,
                    schedule,
                    false,
                    trial.selected_backend(),
                    if schedule {
                        &schedule_replay
                    } else {
                        &trial.det_opts.det_config
                    },
                )
            };
            let check = |path: &Path, label: &str| -> bool {
                let written = PreemptionReader::try_new(path)
                    .unwrap_or_else(|error| panic!("{label}: {error:#}"))
                    .into_inner();
                written.validate().unwrap();
                let keeps_offset = written.rows_carry_instruction_offsets();
                assert_eq!(
                    written.format_version(),
                    keeps_offset.then_some(INSTRUCTION_OFFSET_ROWS_FORMAT),
                    "{label}"
                );
                // Formats 1 to 4, all a Hermit before the offsets reads.
                let read_before_offsets = written
                    .format_version()
                    .is_none_or(|format| (2..INSTRUCTION_OFFSET_ROWS_FORMAT).contains(&format));
                assert_eq!(read_before_offsets, !keeps_offset, "{label}");
                assert_eq!(written.timing_format(), None, "{label}");
                assert!(!written.lists_its_time_skips(), "{label}");
                assert!(!written.charges_replayed_switch_ins(), "{label}");
                assert_eq!(written.time_skips(), None, "{label}");
                let mut rows = written.extract_all().remove(&tid).unwrap().into_iter();
                while let Some((time, _, rcbs, offset)) = rows.next_with_offset() {
                    assert_eq!(source_rows.get(&time), Some(&(rcbs, offset)), "{label}");
                }
                let recorded = detcore::preemptions::read_recorded_offsets(path).unwrap();
                assert_eq!(
                    recorded,
                    if keeps_offset {
                        RecordedOffsets::RowsOnly
                    } else {
                        RecordedOffsets::None
                    },
                    "{label}"
                );
                // A minimization trial is a preemption replay of the record.
                assert_eq!(guard(recorded, false), None, "{label}");
                let schedule = guard(recorded, true);
                if keeps_offset {
                    assert!(
                        schedule.as_deref().is_some_and(|why| why.contains(
                            "its schedule trace does not record the instruction offsets its rows \
                             hold"
                        ) && why
                            .contains("replay it with --replay-preemptions-from")),
                        "{label}: {schedule:?}"
                    );
                } else {
                    assert_eq!(schedule, None, "{label}");
                }
                keeps_offset
            };
            // Analyze's input check, on the input of this minimization.
            options
                .clone()
                .adopt_instruction_offsets(&[("--run1-preemptions", &preempts_path)])
                .unwrap();
            let mut trials = 0;
            let mut kept_offset = false;
            let (_, minimal_path, _) = options
                .minimize_with(&preempts_path, |runname, trial_preempts| {
                    trials += 1;
                    kept_offset |= check(trial_preempts, runname);
                    let trial = PreemptionReader::new(trial_preempts).load_all();
                    let history = trial.extract_all().remove(&tid).unwrap();
                    let mut rows = history.into_iter();
                    let mut has_critical = false;
                    while let Some((time, _, _, _)) = rows.next_with_offset() {
                        has_critical |= time == LogicalTime::from_nanos(5_000);
                    }
                    Ok(has_critical)
                })
                .unwrap();
            assert!(trials > 1);
            assert_eq!(kept_offset, with_offsets);
            assert_eq!(check(&minimal_path, "minimal"), with_offsets);
            // The minimal record given back to analyze as an input.
            let fed_back = options
                .clone()
                .adopt_instruction_offsets(&[("--run1-preemptions", &minimal_path)]);
            if with_offsets {
                let error = fed_back.unwrap_err();
                assert!(
                    error
                        .downcast_ref::<crate::app::container::PolicyRefusal>()
                        .is_some()
                        && format!("{error:#}").contains(
                            "its schedule trace does not record the instruction offsets its rows \
                             hold"
                        ),
                    "{error:#}"
                );
            } else {
                fed_back.unwrap();
            }
        }
    }

    /// A minimization trial that `--max-log-bytes` ended stops the
    /// minimization and returns that error, still a `LogCapExceeded`, so main
    /// reports it as the cap (123). Before, any trial's error panicked here.
    #[test]
    fn a_failing_minimize_trial_ends_minimization_with_its_error() {
        let workspace = tempfile::tempdir().unwrap();
        let preempts_path = workspace.path().join("target.preempts");
        // One thread with eight interventions: increasing times, ordinary
        // priorities. When every trial matches, minimizing it takes three
        // trials (batches of 2, 3 and 3 interventions).
        let history: Vec<(LogicalTime, Priority)> = (1..=8)
            .map(|k| (LogicalTime::from_nanos(k * 1000), FIRST_PRIORITY + k))
            .collect();
        PreemptionRecord::from_vecs(&BTreeMap::from([(DetTid::from_raw(3), history)]))
            .write_to_disk(&preempts_path)
            .unwrap();
        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--analyze-seed=7", "--", "/bin/true"])
                .unwrap();

        let mut total_trials = 0;
        options
            .minimize_with(&preempts_path, |_runname, _trial_preempts| {
                total_trials += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(total_trials, 3);

        for failing_trial in 1..=total_trials {
            let mut trials = 0;
            let result = options.minimize_with(&preempts_path, |_runname, trial_preempts| {
                trials += 1;
                assert!(
                    trial_preempts.exists(),
                    "each trial's preemption file is written before it runs"
                );
                if trials == failing_trial {
                    return Err(anyhow::Error::new(LogCapExceeded));
                }
                // An earlier trial finished and still met the criteria.
                Ok(true)
            });
            let Err(error) = result else {
                panic!("trial {failing_trial} failed, yet minimization returned a record")
            };
            assert!(
                error.downcast_ref::<LogCapExceeded>().is_some(),
                "trial {failing_trial}: the error lost its type: {error:#}"
            );
            assert_eq!(
                trials, failing_trial,
                "minimization ran more trials after trial {failing_trial} failed"
            );
        }
    }

    /// When a cut that took a thread's last untested interventions fails,
    /// those go back to be tested again in smaller batches, so the result
    /// holds only interventions found critical. Before, the thread was not
    /// taken up again: its interventions stayed in the "minimized" record
    /// untested, and the count printed left them out.
    #[test]
    fn a_failed_cut_of_a_threads_last_interventions_is_tested_again() {
        let workspace = tempfile::tempdir().unwrap();
        let preempts_path = workspace.path().join("target.preempts");
        let critical = FIRST_PRIORITY + 1;
        let history: Vec<(LogicalTime, Priority)> = (1..=8)
            .map(|k| (LogicalTime::from_nanos(k * 1000), FIRST_PRIORITY + k))
            .collect();
        PreemptionRecord::from_vecs(&BTreeMap::from([(DetTid::from_raw(3), history)]))
            .write_to_disk(&preempts_path)
            .unwrap();
        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--analyze-seed=7", "--", "/bin/true"])
                .unwrap();
        let has_critical = |record: &PreemptionRecord| {
            record
                .as_vecs()
                .values()
                .any(|history| history.iter().any(|(_, prio)| *prio == critical))
        };
        let (minimized, _, _) = options
            .minimize_with(&preempts_path, |_runname, trial_preempts| {
                Ok(has_critical(
                    &PreemptionReader::new(trial_preempts).load_all(),
                ))
            })
            .unwrap();
        let left: Vec<Priority> = minimized
            .as_vecs()
            .values()
            .flat_map(|history| history.iter().map(|(_, prio)| *prio))
            .collect();
        assert_eq!(left, [critical], "{minimized}");
    }
}

#[cfg(test)]
mod replay_departure_tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn minimizing_a_marked_record_never_projects_it_into_unmarked_trials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        let history = (1..=8)
            .map(|i| (LogicalTime::from_nanos(i * 1000), FIRST_PRIORITY + i))
            .collect();
        let record = PreemptionRecord::from_vecs(&BTreeMap::from([(DetTid::from_raw(3), history)]));
        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--analyze-seed=7", "--", "/bin/true"])
                .unwrap();
        for marker in [None, Some(7)] {
            record
                .clone()
                .retain_replay_departure(marker)
                .write_to_disk(&path)
                .unwrap();
            let mut calls = 0;
            let result = options.minimize_with(&path, |_, _| {
                calls += 1;
                Ok(true)
            });
            if marker.is_some() {
                let error = result
                    .expect_err("marked history must not be reconstructed without its marker");
                assert!(
                    error
                        .downcast_ref::<crate::app::container::PolicyRefusal>()
                        .is_some(),
                    "{error:#}"
                );
                assert_eq!(calls, 0);
            } else {
                result.unwrap();
                assert_eq!(calls, 3);
            }
        }
    }
}
