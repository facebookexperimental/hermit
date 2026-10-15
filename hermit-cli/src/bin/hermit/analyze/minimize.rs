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

use crate::analyze::types::AnalyzeOpts;
use crate::global_opts::GlobalOpts;

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
                    self.stamp_trial_record(PreemptionRecord::from_vecs(&btmap))
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
    use crate::container::LogCapExceeded;

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
}
