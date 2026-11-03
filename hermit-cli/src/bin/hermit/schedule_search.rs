/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;

use anyhow::bail;
use colored::Colorize;
use detcore::edit_distance::BubbleSortIter;
use detcore::edit_distance::NeedlemanWunsch;
use detcore::edit_distance::NeedlemanWunschError;
use detcore::edit_distance::generate_permutation;
use detcore::edit_distance::iterable_bubble_sort_from_perm;
use detcore::types::DetTid;
use detcore::types::Op;
use detcore::types::SchedEvent;

/// The branch-count difference two replays of one control schedule may show
/// on a host whose branch counter is not exact ([`Config::max_branch_count_jitter`]).
pub const INEXACT_COUNTER_BRANCH_JITTER: u32 = 64;

/// User configurable settings.
pub struct Config {
    pub max_jitter_editdist: usize,
    pub max_jitter_swapdist: usize,
    pub max_event_level_search_passes: usize,
    pub max_unmatched_to_print: usize,
    /// Bound quadratic traceback work before falling back to event permutation search.
    pub max_needleman_matrix_cells: usize,
    /// How many replays of a jittered control schedule may be run before the
    /// search gives up; the schedule is classified once two of them agree.
    pub max_replay_attempts: usize,
    /// How many retired conditional branches two replays of one control schedule
    /// may disagree by in a branch event and still count as the same realized
    /// trace, or `None` to require identical traces, event times included. Only
    /// a host whose branch counter is not exact (AMD Zen with SpecLockMap
    /// enabled, which a VM guest cannot turn off) moves a preemption by a few
    /// branches between runs while the interleaving stays the same; elsewhere
    /// such a difference is replay nondeterminism. analyze sets it on such a
    /// host only ([`INEXACT_COUNTER_BRANCH_JITTER`]).
    pub max_branch_count_jitter: Option<u32>,
    /// Try to split a final adjacent event pair into a precise branch boundary.
    pub refine_sub_events: bool,
    pub verbose: bool,
    /// Activate a search that uses Needleman Wunsch alignment during each step.
    pub needleman_search: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_jitter_editdist: 0,
            max_jitter_swapdist: 0,
            max_event_level_search_passes: 100,
            max_unmatched_to_print: 10,
            max_needleman_matrix_cells: 4_000_000,
            max_replay_attempts: 4,
            max_branch_count_jitter: None,
            refine_sub_events: true,
            verbose: false,
            needleman_search: false,
        }
    }
}

/// What one trial of the search reports: whether the replayed schedule passed,
/// and the schedule the run actually realized. An `Err` is a trial that could
/// not finish, for example one that `--max-log-bytes` ended; the search makes
/// no further trial and returns that error unchanged, so its type (such as
/// `LogCapExceeded`) still reaches the top-level handler.
pub type TrialOutcome = anyhow::Result<(bool, Vec<SchedEvent>)>;

struct EventLevelSearchResult {
    passing_schedule: Vec<SchedEvent>,
    failing_schedule: Vec<SchedEvent>,
}

/// The bisection stopped with unmatched events and has not established an
/// exact one-swap pair of schedules with opposite outcomes.
#[derive(Debug)]
pub struct NoSingleSwap {
    /// How many events only one of the two schedules has, counting both
    /// sides.
    pub unshared_events: usize,
    /// How many reorderings of shared events are left between the two: 1
    /// when the search stopped one swap apart, 0 when the two run their
    /// shared events in the same order from the start.
    pub swaps_left: usize,
    /// Whether a trial of the failing schedule without the events the
    /// passing one lacks ran, and passed.
    pub trimmed_failing_passed: bool,
}

impl std::fmt::Display for NoSingleSwap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.swaps_left == 0 {
            write!(
                f,
                "the passing and failing schedules run the events they share in the same order \
                 and differ in {} events that only one of them has.",
                self.unshared_events
            )?;
        } else {
            write!(
                f,
                "the search stopped one swap from the passing schedule, but the two schedules \
                 still differ in {} events that only one of them has{}.",
                self.unshared_events,
                if self.trimmed_failing_passed {
                    ", and the failing schedule without the events only it has no longer fails"
                } else {
                    ""
                }
            )?;
        }
        write!(
            f,
            " The search has not established an exact one-swap pair of schedules with opposite \
             outcomes. `hermit analyze --prefix-search` needs no baseline schedule and searches \
             for a scheduling decision within the failing one."
        )
    }
}

impl std::error::Error for NoSingleSwap {}

#[derive(Debug)]
pub struct CriticalSchedule {
    pub failing_schedule: Vec<SchedEvent>,
    pub passing_schedule: Vec<SchedEvent>,

    /// The index of an event in the failing schedule. If you swap this event with the one before it, it
    /// changes the outcome to passing, and should in fact match the passing schedule.
    pub critical_event_index: usize,
}

/// return 2 if first == second (ignores end_time)
/// return -1 if only count is different
/// return -2 otherwise
pub fn scoring_function(first: SchedEvent, second: SchedEvent) -> i32 {
    let mut score = -2;
    if first.same_kind(&second) {
        score += 1;
    } else {
        return score;
    }
    if first.count == second.count {
        score += 3;
    }
    score
}

#[allow(dead_code)]
/// Search for a schedule which is failing the criteria, but is edit distance one from succeeding.
///
/// A trial that returns `Err` ends the search: no further trial runs and the
/// error is returned as it is.
pub fn search_for_critical_schedule<F>(
    mut tester: F,
    initial_passing_schedule: Vec<SchedEvent>,
    initial_failing_schedule: Vec<SchedEvent>,
    cfg: &Config,
) -> anyhow::Result<CriticalSchedule>
where
    F: FnMut(&[SchedEvent]) -> TrialOutcome,
{
    eprintln!("Verifying pass/fail endpoints of the search, using schedule-trace replay:");
    let (passing_endpoint_passes, _) = tester(&initial_passing_schedule)?;
    if !passing_endpoint_passes {
        bail!("the passing schedule endpoint did not reproduce a passing outcome");
    }
    let (failing_endpoint_passes, _) = tester(&initial_failing_schedule)?;
    if failing_endpoint_passes {
        bail!("the failing schedule endpoint did not reproduce the target failure");
    }

    // Do the first level search at the event level
    let EventLevelSearchResult {
        passing_schedule,
        failing_schedule,
    } = if cfg.needleman_search {
        needleman_level_search(
            &mut tester,
            initial_passing_schedule,
            initial_failing_schedule,
            cfg,
        )?
    } else {
        event_level_search(
            &mut tester,
            initial_passing_schedule,
            initial_failing_schedule,
            cfg,
        )?
    };

    if cfg.refine_sub_events
        && let Some(critical_schedule) = sub_event_search(
            &mut tester,
            passing_schedule.clone(),
            failing_schedule.clone(),
        )?
    {
        return Ok(critical_schedule);
    }

    eprintln!(
        ":: {}",
        "Critical events found which exercise race bug."
            .green()
            .bold()
    );
    let critical_event_index = {
        let (common_prefix, _) = get_common_pre_and_postfix(
            &passing_schedule,
            &failing_schedule,
            SchedEvent::same_operations,
        );
        common_prefix.len() + 1
    };
    eprintln!("Critical event index {}", critical_event_index);
    Ok(CriticalSchedule {
        failing_schedule,
        passing_schedule,
        critical_event_index,
    })
}

/// Each Branch event of `schedule` with its thread's total branch count at
/// its end: the point in the thread's own execution the event ends at, the
/// same in every run of the same thread.
fn branch_totals(schedule: &[SchedEvent]) -> Vec<u64> {
    let mut branches = HashMap::new();
    schedule
        .iter()
        .map(|event| {
            if event.op != Op::Branch {
                return 0;
            }
            let thread_total: &mut u64 = branches.entry(event.dettid).or_default();
            *thread_total += u64::from(event.count);
            *thread_total
        })
        .collect()
}

/// The points (thread, branch total) at which a Branch event of either
/// schedule ends a timeslice taken without a stop (`SchedEvent::stopless`):
/// there that run read no `end_rip`, so no RIP can be compared.
fn stopless_points(schedules: [&[SchedEvent]; 2]) -> HashSet<(DetTid, u64)> {
    schedules
        .into_iter()
        .flat_map(|schedule| {
            schedule
                .iter()
                .zip(branch_totals(schedule))
                // A record that marks an end it holds a RIP for is refused
                // where records are read (`SchedEvent::stopless_mark_refusal`);
                // should one reach here, its known RIP is still compared.
                .filter(|(event, _)| event.stopless && event.end_rip.is_none())
                .map(|(event, total)| (event.dettid, total))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A schedule's alignment keys: each event with its thread's branch total
/// at its end (`alignment_keys`).
type AlignmentKeys = Vec<(SchedEvent, u64)>;

/// What two runs' schedules are aligned on (`generate_permutation`): each
/// event as recorded, except that a Branch event stands for the branches it
/// covers, as its thread's total branch count at its end, rather than for how
/// the run took the timeslice end there. A stop reads an `end_rip` that an
/// end taken without one (`SchedEvent::stopless`) does not have, and two runs
/// differ in which ends their thread ran alone at: at a point in `unknown`,
/// where either run took the end without a stop, the RIP is left out. Two
/// known RIPs at the same point are compared, so ends at different
/// instructions stay apart. The total also keeps apart batches of the same
/// size that the RIP told apart.
fn alignment_keys(schedule: &[SchedEvent], unknown: &HashSet<(DetTid, u64)>) -> AlignmentKeys {
    schedule
        .iter()
        .zip(branch_totals(schedule))
        .map(|(event, total)| {
            let mut key = event.clone();
            key.stopless = false;
            if event.op == Op::Branch && unknown.contains(&(event.dettid, total)) {
                key.end_rip = None;
            }
            (key, total)
        })
        .collect()
}

/// The `alignment_keys` of `source` and `target`, each with the RIPs left out
/// where either took a slice end without a stop.
fn aligned_keys(source: &[SchedEvent], target: &[SchedEvent]) -> (AlignmentKeys, AlignmentKeys) {
    let unknown = stopless_points([source, target]);
    (
        alignment_keys(source, &unknown),
        alignment_keys(target, &unknown),
    )
}

/// Bubble sort `source` into `target`'s order, matching events by
/// `aligned_keys`.
fn align<'a>(source: &'a [SchedEvent], target: &[SchedEvent]) -> BubbleSortIter<'a, SchedEvent> {
    let (source_keys, target_keys) = aligned_keys(source, target);
    let permutation = generate_permutation(&source_keys, &target_keys);
    iterable_bubble_sort_from_perm(source, permutation)
}

// Return edit distance and swap distance.
fn just_distance(sched1: &[SchedEvent], sched2: &[SchedEvent]) -> (usize, usize) {
    let bubbles = align(sched1, sched2);
    (bubbles.edit_distance(), bubbles.swap_distance())
}

/// `schedule` without the events `other` has no counterpart for, matched as
/// multisets from the left, if what is left differs from `other` by exactly one
/// adjacent swap. Counterparts are matched by the alignment policy
/// (`aligned_keys`), as `align` matches them: a slice end one schedule took
/// without a stop and the other took with one are the same event, so the
/// shared events are the ones alignment would pair, with `schedule`'s own
/// events retained.
fn events_shared_with(schedule: &[SchedEvent], other: &[SchedEvent]) -> Option<Vec<SchedEvent>> {
    let (schedule_keys, other_keys) = aligned_keys(schedule, other);
    let mut available: std::collections::HashMap<&SchedEvent, usize> =
        std::collections::HashMap::new();
    for (key, _) in &other_keys {
        *available.entry(key).or_default() += 1;
    }
    let shared: Vec<SchedEvent> = schedule
        .iter()
        .zip(&schedule_keys)
        .filter(|(_, (key, _))| match available.get_mut(key) {
            Some(count) if *count > 0 => {
                *count -= 1;
                true
            }
            _ => false,
        })
        .map(|(event, _)| event.clone())
        .collect();
    (shared.len() == other.len() && just_distance(&shared, other) == (1, 1)).then_some(shared)
}

/// Whether two realized traces of one control schedule are the same interleaving:
/// the same events of the same threads in the same order, with branch counts that
/// differ by at most `branch_jitter`. Event times are not compared: they follow
/// from the counts, so a branch-count difference shifts every later time.
fn same_interleaving(first: &[SchedEvent], second: &[SchedEvent], branch_jitter: u32) -> bool {
    first.len() == second.len()
        && first.iter().zip(second).all(|(a, b)| {
            let allowed = if a.op == Op::Branch { branch_jitter } else { 0 };
            a.dettid == b.dettid
                && a.op == b.op
                && a.start_rip == b.start_rip
                && a.end_rip == b.end_rip
                && a.count.abs_diff(b.count) <= allowed
        })
}

/// How two realized traces of one control schedule compare
/// ([`replay_agreement`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayAgreement {
    /// The same trace, event times included.
    Identical,
    /// The same interleaving, whose branch counts differ within the allowed
    /// jitter, and so whose event times differ too.
    Within(ReplayDifference),
    /// Another trace.
    Different,
}

/// How far apart two realized traces of one interleaving are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReplayDifference {
    /// The largest difference in a branch event's count.
    max_branches: u32,
    /// The largest difference in an event's end time, in nanoseconds.
    max_end_time_ns: u64,
}

/// Whether two realized traces of one control schedule count as the same
/// trace. Without `branch_jitter` (a host whose branch counter is exact) only
/// identical traces do, event times included. With it, so does the same
/// interleaving whose branch counts differ by at most that much
/// ([`same_interleaving`]).
fn replay_agreement(
    first: &[SchedEvent],
    second: &[SchedEvent],
    branch_jitter: Option<u32>,
) -> ReplayAgreement {
    if first == second {
        return ReplayAgreement::Identical;
    }
    match branch_jitter {
        Some(jitter) if same_interleaving(first, second, jitter) => {
            let pairs = || first.iter().zip(second);
            ReplayAgreement::Within(ReplayDifference {
                max_branches: pairs()
                    .map(|(a, b)| a.count.abs_diff(b.count))
                    .max()
                    .unwrap_or(0),
                max_end_time_ns: pairs()
                    .filter_map(|(a, b)| {
                        Some(a.end_time?.as_nanos().abs_diff(b.end_time?.as_nanos()))
                    })
                    .max()
                    .unwrap_or(0),
            })
        }
        _ => ReplayAgreement::Different,
    }
}

/// What analyze says when it accepts two replays of one control schedule that
/// are the same interleaving but not the same realized trace.
fn accepted_replay_difference(
    attempt: usize,
    difference: ReplayDifference,
    branch_jitter: u32,
) -> String {
    format!(
        "Jittered control schedule reproduced the same outcome and interleaving on attempt \
         {attempt}, but not the same realized trace: branch counts differ by up to {} and \
         event end times by up to {} ns. Accepted, as this host's branch counter is not exact \
         (up to {branch_jitter} branches allowed)",
        difference.max_branches, difference.max_end_time_ns,
    )
}

fn test_and_select_stable_point<F>(
    tester: &mut F,
    requested_schedule: &[SchedEvent],
    cfg: &Config,
) -> TrialOutcome
where
    F: FnMut(&[SchedEvent]) -> TrialOutcome,
{
    let (passes, actual_schedule) = tester(requested_schedule)?;
    let jitter = just_distance(requested_schedule, &actual_schedule);
    if jitter.0 <= cfg.max_jitter_editdist && jitter.1 <= cfg.max_jitter_swapdist {
        return Ok((passes, actual_schedule));
    }

    eprintln!(
        ":: {}",
        format!(
            "Replay jitter {}:{} exceeded threshold {}:{}; checking control-schedule stability",
            jitter.0, jitter.1, cfg.max_jitter_editdist, cfg.max_jitter_swapdist,
        )
        .yellow()
        .bold(),
    );

    // Any two replays that agree classify the schedule: on a host whose
    // branch counter drifts, the first replay can be the odd one out.
    let mut replays = vec![(passes, actual_schedule)];
    for attempt in 2..=cfg.max_replay_attempts.max(2) {
        let (replayed_passes, replayed_actual) = tester(requested_schedule)?;
        if let Some((agreed, agreement)) =
            replays
                .iter()
                .enumerate()
                .find_map(|(index, (earlier_passes, earlier_actual))| {
                    let agreement = replay_agreement(
                        &replayed_actual,
                        earlier_actual,
                        cfg.max_branch_count_jitter,
                    );
                    (replayed_passes == *earlier_passes && agreement != ReplayAgreement::Different)
                        .then_some((index, agreement))
                })
        {
            match agreement {
                ReplayAgreement::Within(difference) => eprintln!(
                    ":: {} (agreeing with attempt {})",
                    accepted_replay_difference(
                        attempt,
                        difference,
                        cfg.max_branch_count_jitter.unwrap_or(0)
                    )
                    .yellow(),
                    agreed + 1,
                ),
                _ => eprintln!(
                    ":: Jittered control schedule reproduced the same outcome and realized trace on attempt {} (agreeing with attempt {})",
                    attempt,
                    agreed + 1,
                ),
            }
            return Ok((replayed_passes, requested_schedule.to_vec()));
        }
        replays.push((replayed_passes, replayed_actual));
    }

    anyhow::bail!(
        "jittered schedule replay was not stable after {} attempts; refusing to attach an outcome \
         to the requested control schedule. Replays of one schedule realized different traces \
         ({}), so this program's schedule cannot be replayed reproducibly here; on a host whose \
         branch counter is inexact (see `hermit host-capabilities`) branch counts can drift \
         between runs",
        cfg.max_replay_attempts.max(2),
        match cfg.max_branch_count_jitter {
            Some(jitter) => format!("more than {jitter} branches apart or in another order"),
            None => "not identical, on a host whose branch counter is exact".to_owned(),
        },
    );
}

/// Search of the schedule space to find the critical schedule
fn needleman_level_search<F>(
    tester: &mut F,
    mut passing_schedule: Vec<SchedEvent>,
    mut failing_schedule: Vec<SchedEvent>,
    cfg: &Config,
) -> anyhow::Result<EventLevelSearchResult>
where
    F: FnMut(&[SchedEvent]) -> TrialOutcome,
{
    for pass_number in 0..cfg.max_event_level_search_passes {
        let (edit_distance, swap_distance) = just_distance(&passing_schedule, &failing_schedule);
        if edit_distance == 1 && swap_distance == 1 {
            return Ok(EventLevelSearchResult {
                passing_schedule,
                failing_schedule,
            });
        }
        if edit_distance != swap_distance {
            eprintln!(
                ":: Needleman-Wunsch endpoints contain {} insertion/deletion edits; falling back to event permutation search",
                edit_distance - swap_distance,
            );
            return event_level_search(tester, passing_schedule, failing_schedule, cfg);
        }

        let mut needleman = NeedlemanWunsch {
            first_sequence: passing_schedule.clone(),
            second_sequence: failing_schedule.clone(),
            ..Default::default()
        };
        let (start_index, global_alignment) = match needleman.match_sequences_bounded(
            Some(scoring_function),
            None,
            Some(-2),
            cfg.max_needleman_matrix_cells,
        ) {
            Ok(alignment) => alignment,
            Err(NeedlemanWunschError::MatrixTooLarge { cells, max_cells }) => {
                eprintln!(
                    ":: Needleman-Wunsch alignment requires {} cells (limit {}); falling back to event permutation search",
                    cells, max_cells,
                );
                return event_level_search(tester, passing_schedule, failing_schedule, cfg);
            }
        };
        if global_alignment.is_empty() {
            eprintln!(
                ":: Needleman-Wunsch produced no alignment difference on pass {}; falling back to event permutation search",
                pass_number,
            );
            return event_level_search(tester, passing_schedule, failing_schedule, cfg);
        }
        let requested_midpoint_schedule =
            needleman.generate_midpoint_schedule(start_index, global_alignment);

        let distance_from_passing = just_distance(&passing_schedule, &requested_midpoint_schedule);
        let distance_from_failing = just_distance(&requested_midpoint_schedule, &failing_schedule);
        if distance_from_passing >= (edit_distance, swap_distance)
            || distance_from_failing >= (edit_distance, swap_distance)
        {
            eprintln!(
                ":: Needleman-Wunsch midpoint did not strictly reduce both sides of the {}:{} interval; falling back to event permutation search",
                edit_distance, swap_distance,
            );
            return event_level_search(tester, passing_schedule, failing_schedule, cfg);
        }

        let (midpoint_passes, selected_new_point) =
            test_and_select_stable_point(tester, &requested_midpoint_schedule, cfg)?;

        if midpoint_passes {
            passing_schedule = selected_new_point;
        } else {
            failing_schedule = selected_new_point;
        }
    }

    anyhow::bail!(
        "event-level search failed: no convergence after {} passes",
        cfg.max_event_level_search_passes
    );
}

/// Generate a permutation between source and target and print unmatched events (up to a limit).
fn print_unmatched_events(
    passing_schedule: &[SchedEvent],
    failing_schedule: &[SchedEvent],
    max_to_print: usize,
) {
    let (passing_keys, failing_keys) = aligned_keys(passing_schedule, failing_schedule);
    let perm = generate_permutation(&passing_keys, &failing_keys);
    for ix in perm.unmatched_source_indices.iter().take(max_to_print) {
        eprintln!(
            " :: unmatched source evt #{}: {}",
            ix, passing_schedule[*ix]
        );
    }
    let unmatched_src = perm.unmatched_source_indices.len();
    if unmatched_src > max_to_print {
        eprintln!(
            " :: ... plus {} more unmatched events",
            unmatched_src - max_to_print,
        )
    }

    for ix in perm.unmatched_target_indices.iter().take(max_to_print) {
        eprintln!(
            " :: unmatched target evt #{}: {}",
            ix, failing_schedule[*ix]
        );
    }
    let unmatched_trg = perm.unmatched_target_indices.len();
    if unmatched_trg > max_to_print {
        eprintln!(
            " :: ... plus {} more unmatched events",
            unmatched_trg - max_to_print,
        )
    }
}

/// Perform a multi-level search of the schedule space to find the critical schedule
/// between the two starting schedules by evaluating the given function at each step.
/// The result will be a failing schedule and coordinates in that schedule for the
/// adjacent branches that when flipped will cause the test to fail
fn event_level_search<F>(
    tester: &mut F,
    mut passing_schedule: Vec<SchedEvent>,
    mut failing_schedule: Vec<SchedEvent>,
    cfg: &Config,
) -> anyhow::Result<EventLevelSearchResult>
where
    F: FnMut(&[SchedEvent]) -> TrialOutcome,
{
    let orig_passing_schedule = passing_schedule.clone();
    let orig_failing_schedule = failing_schedule.clone();

    for pass_number in 0..cfg.max_event_level_search_passes {
        let (swap_dist, edit_dist, requested_midpoint_schedule) = {
            let mut bubbles = align(&passing_schedule, &failing_schedule);
            (
                bubbles.swap_distance(),
                bubbles.edit_distance(),
                bubbles.midpoint().cloned().collect::<Vec<_>>(),
            )
        };
        eprintln!(
            "\n:: Event-Level Search Pass {} => EditDistance = {}, Swap Distance = {} ({:.0}% matched, midpoint sched len = {})",
            pass_number,
            edit_dist,
            swap_dist,
            (100.0 * 2.0 * requested_midpoint_schedule.len() as f64
                / (passing_schedule.len() + failing_schedule.len()) as f64),
            requested_midpoint_schedule.len()
        );

        let unmatched_total =
            passing_schedule.len() + failing_schedule.len() - 2 * requested_midpoint_schedule.len();
        if cfg.verbose && unmatched_total > 0 {
            print_unmatched_events(
                &passing_schedule,
                &failing_schedule,
                cfg.max_unmatched_to_print,
            );
        }

        if swap_dist == 0 && edit_dist == 0 {
            bail!(
                "the passing and failing schedules are the same sequence of events, yet their \
                 replays have opposite outcomes: the outcome does not follow the schedule. Check \
                 that the target replays the same (--selfcheck), and look for inputs Hermit does \
                 not make deterministic."
            );
        }
        if swap_dist == 0 {
            return Err(anyhow::Error::new(NoSingleSwap {
                unshared_events: edit_dist,
                swaps_left: 0,
                trimmed_failing_passed: false,
            }));
        }

        if swap_dist == 1 && edit_dist == 1 {
            return Ok(EventLevelSearchResult {
                passing_schedule,
                failing_schedule,
            });
        }

        if swap_dist == 1 {
            // The failing run took a path the passing one did not (an error branch, a
            // retry), so its schedule keeps events the passing one lacks. Without them
            // the two differ by the one remaining swap; that order is the critical
            // pair if it still fails. It is selected like every other midpoint:
            // within the jitter bound, or stable across replays, and the trace it
            // selects is what the search reports.
            let mut trimmed_failing_passed = false;
            if let Some(trimmed) = events_shared_with(&failing_schedule, &passing_schedule) {
                eprintln!(
                    ":: Schedules are one swap apart; testing the failing schedule without its {} events the passing one lacks",
                    failing_schedule.len() - trimmed.len(),
                );
                let (trimmed_passes, selected) =
                    test_and_select_stable_point(tester, &trimmed, cfg)?;
                if !trimmed_passes {
                    return Ok(EventLevelSearchResult {
                        passing_schedule,
                        failing_schedule: selected,
                    });
                }
                trimmed_failing_passed = true;
            }
            return Err(anyhow::Error::new(NoSingleSwap {
                unshared_events: edit_dist - swap_dist,
                swaps_left: 1,
                trimmed_failing_passed,
            }));
        }

        let (midpoint_passes, selected_new_point) =
            test_and_select_stable_point(tester, &requested_midpoint_schedule, cfg)?;
        if cfg.verbose {
            let (jitter_edit, jitter_swap) =
                just_distance(&requested_midpoint_schedule, &selected_new_point);
            if jitter_edit > 0 || jitter_swap > 0 {
                eprintln!(
                    ":: Jitter was {}:{} edit/swap distance (requested synthetic schedule vs actual schedule)",
                    jitter_edit, jitter_swap
                );
            } else {
                eprintln!(
                    ":: No jitter in this run (requested synthetic schedule identical to actual schedule)",
                );
            }
            let (edit1, swap1) = just_distance(&passing_schedule, &selected_new_point);
            let (edit2, swap2) = just_distance(&selected_new_point, &failing_schedule);
            eprintln!(
                ":: Including jitter, actual run was {}:{} from passing pole and {}:{} from failing one",
                edit1, swap1, edit2, swap2
            );
            let (edit1, swap1) = just_distance(&orig_passing_schedule, &passing_schedule);
            let (edit2, swap2) = just_distance(&failing_schedule, &orig_failing_schedule);
            eprintln!(
                ":: Note, those poles are {}:{} from original passing, and {}:{} from original failing respectively.",
                edit1, swap1, edit2, swap2
            );
        }

        if midpoint_passes {
            passing_schedule = selected_new_point;
        } else {
            failing_schedule = selected_new_point;
        }
    }

    anyhow::bail!(
        "event-level search failed: no convergence after {} passes",
        cfg.max_event_level_search_passes
    );
}

/// Returns `(prefix, postfix)` respectively, of events that are `same`.
fn get_common_pre_and_postfix<'a, T>(
    sched_1: &'a [T],
    sched_2: &[T],
    same: impl Fn(&T, &T) -> bool,
) -> (&'a [T], &'a [T]) {
    let common_prefix_len = sched_1
        .iter()
        .zip(sched_2.iter())
        .take_while(|(e1, e2)| same(e1, e2))
        .count();

    let common_postfix_len = if sched_1.len() == sched_2.len() && sched_1.len() == common_prefix_len
    {
        0
    } else {
        sched_1
            .iter()
            .rev()
            .zip(sched_2.iter().rev())
            .take_while(|(e1, e2)| same(e1, e2))
            .count()
    };

    (
        &sched_1[0..common_prefix_len],
        &sched_1[(sched_1.len() - common_postfix_len)..],
    )
}

/// This function performs a search for the critical branch between two schedules that only differ
/// by swap-distance 1 (in terms of events).
///
/// Let's start with some definitions. We are going to represent the schedules' differeing ROIs as
/// [ {ThreadLetter}_{BranchCount}, .. ], thread A with 50 branches as A_50. Let's say the ROI for
/// the passing schedule is [ A_x, B_y ], and for the failing schedule it's [ B_z, A_w ].
///
/// That implies there is some branch `j` in thread B that when shifted to execute before a
/// branch `i` in thread A, the test will fail. Our job is to find the critical pair `i` and `j`
/// To accomplish this, we will perform 2 different binary searches. The first will search over all
/// schedules of the form
///
/// [A_i, B_q, A_k] where i + k = max(x, w) and q = max(y, z)
///
/// to find the minimum value of i where the executed schedule allows the test to pass. To find the
/// critical branch in thread A which (unfortunately) will be `i + 1`. Next, we do another binary
/// search on schedules of the form
///
/// [A_i, B_j, A_k, B_m] where i + k = max(x, w) and j + m = max(y, z) and i is held constant
///
/// to find the minimum value of j where the executed schedule causes a failure. This tells us the
/// critical branch in thread B which will (fortunately) be `j`. We will return the critical
/// schedule in the form that passes and coordinates into the schedule to indicate where a single
/// flipped pair of branches can cause the test to fail
fn sub_event_search<F>(
    tester: &mut F,
    passing_schedule: Vec<SchedEvent>,
    failing_schedule: Vec<SchedEvent>,
) -> anyhow::Result<Option<CriticalSchedule>>
where
    F: FnMut(&[SchedEvent]) -> TrialOutcome,
{
    let (prefix, postfix) = get_common_pre_and_postfix(
        &passing_schedule,
        &failing_schedule,
        SchedEvent::same_operations,
    );
    if passing_schedule.len() != failing_schedule.len()
        || prefix.len() + postfix.len() + 2 != passing_schedule.len()
    {
        eprintln!(
            ":: Skipping sub-event refinement: final schedules are not a single exact adjacent event reversal"
        );
        return Ok(None);
    }

    let critical_range = Range {
        start: prefix.len(),
        end: prefix.len() + 2,
    };

    eprintln!(
        "Passing Size - {}, Failing Size - {}, Prefix Size - {}, Postfix Size - {}",
        passing_schedule.len(),
        failing_schedule.len(),
        prefix.len(),
        postfix.len(),
    );

    let passing_critical_events = &passing_schedule[critical_range.clone()];
    let failing_critical_events = &failing_schedule[critical_range];
    if !same_event_except_count(&passing_critical_events[0], &failing_critical_events[1])
        || !same_event_except_count(&passing_critical_events[1], &failing_critical_events[0])
    {
        eprintln!(
            ":: Skipping sub-event refinement: adjacent events do not form the same reversed pair"
        );
        return Ok(None);
    }

    let critical_pair_passing = [
        sched_event_with_new_count(
            &passing_critical_events[0],
            u32::max(
                passing_critical_events[0].count,
                failing_critical_events[1].count,
            ),
        ),
        sched_event_with_new_count(
            &passing_critical_events[1],
            u32::max(
                passing_critical_events[1].count,
                failing_critical_events[0].count,
            ),
        ),
    ];

    let i_max = critical_pair_passing[0].count;
    let j_max = critical_pair_passing[1].count;
    if !can_split_event(&critical_pair_passing[0])
        || !can_split_event(&critical_pair_passing[1])
        || !critical_pair_passing
            .iter()
            .any(|event| event.op == detcore::types::Op::Branch && event.count > 1)
    {
        eprintln!(
            ":: Skipping sub-event refinement: critical pair has no splittable branch interval"
        );
        return Ok(None);
    }

    // Find the first A branch count that passes. Its predecessor is the failing boundary.
    let Some(i_plus_one) = binary_search(0..i_max, &mut |i| {
        let schedule =
            create_schedule_with_critical_pair(prefix, postfix, &critical_pair_passing, i, j_max);

        let Some(passed) = test_exact_replay(tester, &schedule)? else {
            return Ok(None);
        };

        eprintln!("Binary Search of A at sample {} -> passed = {}", i, passed);

        Ok(Some(!passed))
    })?
    else {
        return Ok(None);
    };

    let Some(i) = i_plus_one.checked_sub(1) else {
        eprintln!(
            ":: Skipping sub-event refinement: A boundary is already passing at zero branches"
        );
        return Ok(None);
    };

    // Find the first B branch count that fails while holding A at its boundary.
    let Some(j) = binary_search(1..j_max, &mut |j| {
        let schedule =
            create_schedule_with_critical_pair(prefix, postfix, &critical_pair_passing, i, j);

        let Some(passed) = test_exact_replay(tester, &schedule)? else {
            return Ok(None);
        };
        eprintln!("Binary Search of B at sample {} -> passed = {}", j, passed);

        Ok(Some(passed))
    })?
    else {
        return Ok(None);
    };

    // We are defining the critical event as the event index in the passing schedule
    // where the first branch in the event must come after the last branch in the previous
    // event in order for the schedule to fail. The calculation below for that index is a little
    // weird, but all it's doing is saying is:
    //
    // if no branches from the first event in the critical pair are used, the ciritical event
    // will appear one index sooner becase events with zero count are filtered
    let critical_event_index = prefix.len() + 1 + usize::from(i > 0);

    let failing_schedule =
        create_schedule_with_critical_pair(prefix, postfix, &critical_pair_passing, i, j);
    let mut passing_schedule = failing_schedule.clone();
    {
        let tmp = passing_schedule[critical_event_index - 1].clone();
        passing_schedule[critical_event_index - 1] = passing_schedule[critical_event_index].clone();
        passing_schedule[critical_event_index] = tmp;
    }

    if just_distance(&passing_schedule, &failing_schedule) != (1, 1) {
        eprintln!(
            ":: Skipping sub-event refinement: final schedules are not one verified adjacent swap apart"
        );
        return Ok(None);
    }
    // The passing schedule is replayed first, and the failing one only when
    // the passing one still passes.
    let outcomes_preserved = match test_exact_replay(tester, &passing_schedule)? {
        None => return Ok(None),
        Some(false) => false,
        Some(true) => match test_exact_replay(tester, &failing_schedule)? {
            None => return Ok(None),
            Some(failing_passes) => !failing_passes,
        },
    };
    if !outcomes_preserved {
        eprintln!(
            ":: Skipping sub-event refinement: final A/B replay did not preserve passing/failing outcomes"
        );
        return Ok(None);
    }

    eprintln!(
        ":: {}",
        "Critical branch boundary found and verified."
            .green()
            .bold()
    );
    Ok(Some(CriticalSchedule {
        failing_schedule,
        passing_schedule,
        critical_event_index,
    }))
}

fn same_event_except_count(first: &SchedEvent, second: &SchedEvent) -> bool {
    first.same_operations(&SchedEvent {
        count: first.count,
        ..*second
    })
}

fn can_split_event(event: &SchedEvent) -> bool {
    event.count > 0 && (event.op == detcore::types::Op::Branch || event.count == 1)
}

fn test_exact_replay<F>(tester: &mut F, requested: &[SchedEvent]) -> anyhow::Result<Option<bool>>
where
    F: FnMut(&[SchedEvent]) -> TrialOutcome,
{
    let (passes, actual) = tester(requested)?;
    let realized = actual.len() == requested.len()
        && actual
            .iter()
            .zip(requested)
            .all(|(actual, requested)| actual.same_operations(requested));
    if !realized {
        eprintln!(
            ":: Skipping sub-event refinement: replay did not realize a requested branch boundary exactly (requested {} events, realized {})",
            requested.len(),
            actual.len(),
        );
        Ok(None)
    } else {
        Ok(Some(passes))
    }
}

/// Binary search implementation specific to our problem. This will return the smallest value
/// where the predicate returns false. `Ok(None)` means the predicate gave up on a sample; an
/// `Err` from the predicate ends the search and is returned unchanged.
fn binary_search<F>(range: Range<u32>, predicate: &mut F) -> anyhow::Result<Option<u32>>
where
    F: FnMut(u32) -> anyhow::Result<Option<bool>>,
{
    let mut left = range.start;
    let mut right = range.end;
    let mut size = right - left;

    while size > 0 {
        let mid = left + size / 2;
        let Some(test_res) = predicate(mid)? else {
            return Ok(None);
        };

        if test_res {
            left = mid + 1;
        } else {
            right = mid;
        }

        size = right - left;
    }

    Ok(Some(left))
}

/// Create a new SchedEvent with the size replaced with the given value
fn sched_event_with_new_count(original: &SchedEvent, new_count: u32) -> SchedEvent {
    SchedEvent {
        count: new_count,
        ..*original
    }
}

fn sched_event_fragment(
    original: &SchedEvent,
    count: u32,
    keeps_start: bool,
    keeps_end: bool,
) -> SchedEvent {
    SchedEvent {
        count,
        start_rip: keeps_start.then_some(original.start_rip).flatten(),
        end_rip: keeps_end.then_some(original.end_rip).flatten(),
        // Only the fragment that ends where the original did ends at its
        // slice end.
        stopless: keeps_end && original.stopless,
        ..*original
    }
}

/// Create a schedule by slicing and mixing the critical section and inserting it
/// into a vec between the given common prefix and postfix. the values i and j determine
/// how many branches from the ciritical pair to keep before swapping to the other for the
/// remainder. Visually, if we have the ciritical pair A_8 and B_9, then with i=4 and j=7,
/// we get:
///                       <- A8 -><- B9 -->
/// Original Branches  => AAAAAAAABBBBBBBBB
/// Synthetic Branches => AAAABBBBBBBAAAABB
///                       <A4><- B7-><A4>B2
fn create_schedule_with_critical_pair(
    prefix: &[SchedEvent],
    postfix: &[SchedEvent],
    critical_pair_passing: &[SchedEvent],
    i: u32,
    j: u32,
) -> Vec<SchedEvent> {
    let i_remainder = critical_pair_passing[0].count - i;
    let j_remainder = critical_pair_passing[1].count - j;
    let critical_section = [
        sched_event_fragment(&critical_pair_passing[0], i, true, i_remainder == 0),
        sched_event_fragment(&critical_pair_passing[1], j, true, j_remainder == 0),
        sched_event_fragment(&critical_pair_passing[0], i_remainder, i == 0, true),
        sched_event_fragment(&critical_pair_passing[1], j_remainder, j == 0, true),
    ];

    eprintln!(
        "Critical Section -> A{} B{} A{} B{}",
        i, j, i_remainder, j_remainder
    );

    prefix
        .iter()
        .cloned()
        .chain(critical_section.into_iter().filter(|event| event.count > 0))
        .chain(postfix.iter().cloned())
        .collect()
}

#[cfg(test)]
mod tests {

    use detcore::preemptions::PreemptionRecord;
    use detcore::types::LogicalTime;
    use detcore::types::Op;
    use detcore::types::SyscallPhase;
    use reverie::syscalls::Sysno;

    use super::*;

    /// Two runs that differ only in which slice ends they took without a
    /// stop align event for event, and batches of the same size stay apart:
    /// each stands for its thread's branches up to its end.
    #[test]
    fn schedules_align_however_their_slice_ends_were_taken() {
        let a = DetTid::from_raw(3);
        let b = DetTid::from_raw(4);
        let stop = |tid, count, rip| {
            SchedEvent::branches(tid, count)
                .with_end_rip(detcore::types::InstructionPointer::new(rip).unwrap())
        };
        let stopped = vec![
            stop(a, 100, 0x10),
            stop(a, 100, 0x20),
            SchedEvent::syscall(a, Sysno::write, SyscallPhase::Prehook),
            stop(b, 100, 0x30),
        ];
        let mut taken_alone = stopped.clone();
        taken_alone[0] = SchedEvent::branches(a, 100).taken_without_stop();
        taken_alone[1] = SchedEvent::branches(a, 100).taken_without_stop();
        assert_eq!(just_distance(&stopped, &taken_alone), (0, 0));
        assert_eq!(
            get_common_pre_and_postfix(&stopped, &taken_alone, SchedEvent::same_operations)
                .0
                .len(),
            stopped.len()
        );
        let swapped = vec![
            stopped[0].clone(),
            stopped[3].clone(),
            stopped[1].clone(),
            stopped[2].clone(),
        ];
        assert_ne!(just_distance(&stopped, &swapped), (0, 0));
        assert_eq!(
            just_distance(&taken_alone, &swapped),
            just_distance(&stopped, &swapped)
        );
    }

    /// Two Branch events at the same point of their thread with known but
    /// different end RIPs are different events: their distance is not zero,
    /// so a zero-jitter search does not accept a replay that ended a slice at
    /// another instruction as the requested schedule without checking that
    /// replays of it agree. Only a RIP the run did not read (a slice end taken
    /// without a stop) is left out of the comparison.
    #[test]
    fn known_end_rips_that_differ_are_not_aligned_as_equal() {
        let a = DetTid::from_raw(3);
        let stop = |rip| {
            SchedEvent::branches(a, 100)
                .with_end_rip(detcore::types::InstructionPointer::new(rip).unwrap())
        };
        let requested = vec![stop(0x401000)];
        let elsewhere = vec![stop(0x402000)];
        assert_ne!(just_distance(&requested, &elsewhere), (0, 0));
        assert_eq!(just_distance(&requested, &requested.clone()), (0, 0));
        // Either run's stopless end at the same point matches.
        let taken_alone = vec![SchedEvent::branches(a, 100).taken_without_stop()];
        assert_eq!(just_distance(&requested, &taken_alone), (0, 0));
        assert_eq!(just_distance(&taken_alone, &elsewhere), (0, 0));
        // A mark on an end with a known RIP, which no record reader admits,
        // does not hide that RIP either.
        let marked = |events: &[SchedEvent]| {
            events
                .iter()
                .map(|event| SchedEvent {
                    stopless: true,
                    ..event.clone()
                })
                .collect::<Vec<_>>()
        };
        assert_ne!(
            just_distance(&marked(&requested), &marked(&elsewhere)),
            (0, 0)
        );

        // Zero jitter allowed: the replay that ended elsewhere is replayed
        // again before it classifies the schedule.
        let mut calls = 0;
        let mut tester = |_: &[SchedEvent]| -> TrialOutcome {
            calls += 1;
            Ok((true, elsewhere.clone()))
        };
        let cfg = Config::default();
        assert_eq!((cfg.max_jitter_editdist, cfg.max_jitter_swapdist), (0, 0));
        test_and_select_stable_point(&mut tester, &requested, &cfg).unwrap();
        assert_eq!(
            calls, 2,
            "a replay that differs at a known RIP is checked again"
        );
    }

    #[test]
    fn test_common_pre_and_postfix() {
        assert_eq!(
            get_common_pre_and_postfix(b"abABcde", b"abBAcde", |a, b| a == b),
            (b"ab".as_slice(), b"cde".as_slice())
        );
    }

    #[test]
    fn test_binary_search() {
        // Check a bunch of close values to make sure we don't have an off-by-one problem.
        // I also did an exhaustive search up to 100,000 because I was sure this wouldn't work,
        // but it does. Thanks, Rust std library
        let search = |range: Range<u32>, below: u32| {
            binary_search(range, &mut |i| Ok(Some(i < below))).unwrap()
        };
        assert_eq!(search(0..10, 4), Some(4));
        assert_eq!(search(0..11, 4), Some(4));
        assert_eq!(search(0..10, 5), Some(5));
        assert_eq!(search(0..11, 5), Some(5));
        assert_eq!(search(1..100, 49), Some(49));
        assert_eq!(search(2..151, 49), Some(49));
        assert_eq!(search(3..100, 58), Some(58));
        assert_eq!(search(4..151, 58), Some(58));
        assert_eq!(binary_search(0..10, &mut |_| Ok(None)).unwrap(), None);
    }

    #[test]
    fn syscall_pair_stays_at_event_granularity() {
        let first = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let second = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let passing = vec![first.clone(), second.clone()];
        let failing = vec![second, first.clone()];
        let tester = |schedule: &[SchedEvent]| -> TrialOutcome {
            Ok((schedule.first() == Some(&first), schedule.to_vec()))
        };

        let critical = search_for_critical_schedule(
            tester,
            passing.clone(),
            failing.clone(),
            &Config::default(),
        )
        .unwrap();

        assert_eq!(critical.passing_schedule, passing);
        assert_eq!(critical.failing_schedule, failing);
        assert_eq!(critical.critical_event_index, 1);
    }

    #[test]
    fn endpoint_outcomes_are_validated() {
        let first = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let second = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let good = vec![first.clone(), second.clone()];
        let bad = vec![second, first];

        let error = search_for_critical_schedule(
            |schedule| Ok((false, schedule.to_vec())),
            good.clone(),
            bad.clone(),
            &Config::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("passing schedule endpoint"));

        let error = search_for_critical_schedule(
            |schedule| Ok((true, schedule.to_vec())),
            good,
            bad,
            &Config::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("failing schedule endpoint"));
    }

    #[test]
    fn sub_event_search_rejects_jittered_replay() {
        let branch = SchedEvent::branches(DetTid::from_raw(3), 5);
        let syscall = SchedEvent::syscall(DetTid::from_raw(5), Sysno::write, SyscallPhase::Prehook);
        let passing = vec![branch.clone(), syscall.clone()];
        let failing = vec![syscall, branch];
        let extra = SchedEvent::syscall(DetTid::from_raw(7), Sysno::read, SyscallPhase::Prehook);
        let mut tester = |schedule: &[SchedEvent]| -> TrialOutcome {
            let mut actual = schedule.to_vec();
            actual.push(extra.clone());
            Ok((false, actual))
        };

        assert!(
            sub_event_search(&mut tester, passing, failing)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn jittered_control_point_requires_stable_replay() {
        let first = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let second = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let requested = vec![first, second];
        let extra = SchedEvent::syscall(DetTid::from_raw(7), Sysno::getpid, SyscallPhase::Prehook);
        let mut calls = 0;
        let mut tester = |schedule: &[SchedEvent]| -> TrialOutcome {
            calls += 1;
            let mut actual = schedule.to_vec();
            actual.push(extra.clone());
            Ok((true, actual))
        };

        let (passes, selected) =
            test_and_select_stable_point(&mut tester, &requested, &Config::default()).unwrap();

        assert!(passes);
        assert_eq!(selected, requested);
        assert_eq!(calls, 2);
    }

    #[test]
    fn a_failing_schedule_with_extra_events_reduces_to_the_one_swap() {
        let a = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let b = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let c = SchedEvent::syscall(DetTid::from_raw(3), Sysno::getpid, SyscallPhase::Prehook);
        let extra = SchedEvent::syscall(DetTid::from_raw(7), Sysno::close, SyscallPhase::Prehook);
        let passing = vec![a.clone(), b.clone(), c.clone()];
        let failing = vec![b.clone(), extra.clone(), a.clone(), c.clone(), extra];
        assert_eq!(
            events_shared_with(&failing, &passing),
            Some(vec![b, a, c.clone()]),
            "the events only the failing run had are dropped"
        );
        assert_eq!(
            events_shared_with(&passing, &passing),
            None,
            "identical schedules are not one swap apart"
        );
    }

    /// The shared-events trim matches counterparts by the alignment
    /// policy, not by raw equality: a slice end the passing run took
    /// without a stop and the failing run took with one (a known RIP)
    /// is the same event. With raw matching that end would be dropped
    /// as unshared, the trim would come up short, and the one-swap
    /// reduction would report no single swap for a pair it can localize.
    #[test]
    fn a_shared_end_taken_with_and_without_a_stop_survives_the_trim() {
        let a = DetTid::from_raw(3);
        let stopped_end = SchedEvent::branches(a, 100)
            .with_end_rip(detcore::types::InstructionPointer::new(0x401000).unwrap());
        let stopless_end = SchedEvent::branches(a, 100).taken_without_stop();
        let x = SchedEvent::syscall(DetTid::from_raw(5), Sysno::write, SyscallPhase::Prehook);
        let y = SchedEvent::syscall(DetTid::from_raw(7), Sysno::read, SyscallPhase::Prehook);
        let extra = SchedEvent::syscall(DetTid::from_raw(9), Sysno::getpid, SyscallPhase::Prehook);
        let passing = vec![stopless_end, x.clone(), y.clone()];
        let failing = vec![stopped_end.clone(), y.clone(), x.clone(), extra];
        assert_eq!(
            events_shared_with(&failing, &passing),
            Some(vec![stopped_end, y, x]),
            "the trim keeps the failing run's own stopped end and drops only its extra event"
        );
    }

    /// The failing schedule without its extra events is selected like every
    /// other midpoint, through the jitter bound and the replay-stability
    /// check, and the search reports the trace it selected, not the one it
    /// asked for.
    #[test]
    fn the_trimmed_failing_schedule_is_selected_as_a_stable_point() {
        let a = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let b = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let c = SchedEvent::syscall(DetTid::from_raw(3), Sysno::getpid, SyscallPhase::Prehook);
        let extra = SchedEvent::syscall(DetTid::from_raw(7), Sysno::close, SyscallPhase::Prehook);
        let late = SchedEvent::syscall(DetTid::from_raw(9), Sysno::getppid, SyscallPhase::Prehook);
        let passing = vec![a.clone(), b.clone(), c.clone()];
        let failing = vec![b.clone(), a.clone(), c.clone(), extra];
        let trimmed = vec![b.clone(), a, c];
        // Thread 5 first fails; every replay realizes one more event at the end.
        let tester = |calls: &mut Vec<Vec<SchedEvent>>, schedule: &[SchedEvent]| -> TrialOutcome {
            calls.push(schedule.to_vec());
            let mut realized = schedule.to_vec();
            realized.push(late.clone());
            Ok((schedule.first() != Some(&b), realized))
        };

        // Within the jitter bound, the realized trace is reported.
        let mut calls = Vec::new();
        let within = Config {
            max_jitter_editdist: 1,
            max_jitter_swapdist: 1,
            ..Default::default()
        };
        let result = event_level_search(
            &mut |schedule: &[SchedEvent]| tester(&mut calls, schedule),
            passing.clone(),
            failing.clone(),
            &within,
        )
        .unwrap();
        let mut realized = trimmed.clone();
        realized.push(late.clone());
        assert_eq!(result.failing_schedule, realized);
        assert_eq!(calls, std::slice::from_ref(&trimmed));

        // Past it, the trimmed schedule is replayed again for stability first.
        let mut calls = Vec::new();
        let result = event_level_search(
            &mut |schedule: &[SchedEvent]| tester(&mut calls, schedule),
            passing,
            failing,
            &Config::default(),
        )
        .unwrap();
        assert_eq!(result.failing_schedule, trimmed);
        assert_eq!(calls, [trimmed.clone(), trimmed]);
    }

    /// A failure that needs an event only the failing schedule has ends the
    /// search one swap apart with a typed error, which analyze answers with a
    /// prefix search.
    #[test]
    fn a_failure_on_failing_only_events_ends_with_no_single_swap() {
        let a = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let b = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let c = SchedEvent::syscall(DetTid::from_raw(3), Sysno::getpid, SyscallPhase::Prehook);
        let extra = SchedEvent::syscall(DetTid::from_raw(7), Sysno::close, SyscallPhase::Prehook);
        let passing = vec![a.clone(), b.clone(), c.clone()];
        let failing = vec![b, extra.clone(), a, c];
        let tester = |schedule: &[SchedEvent]| -> TrialOutcome {
            Ok((!schedule.contains(&extra), schedule.to_vec()))
        };
        let error =
            search_for_critical_schedule(tester, passing, failing, &Config::default()).unwrap_err();
        let no_swap = error
            .downcast_ref::<NoSingleSwap>()
            .unwrap_or_else(|| panic!("not NoSingleSwap: {error:#}"));
        assert_eq!((no_swap.unshared_events, no_swap.swaps_left), (1, 1));
        // The failing schedule without its own event ran, and passed.
        assert!(no_swap.trimmed_failing_passed);
        let message = error.to_string();
        assert!(
            message.contains("without the events only it has no longer fails")
                && message.contains("--prefix-search"),
            "{message}"
        );

        // An extra event in the passing schedule need not cause the outcome
        // difference: reordering the shared events alone can make it pass.
        let a = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let b = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let passing = vec![a.clone(), extra, b.clone()];
        let failing = vec![b.clone(), a.clone()];
        let tester = |schedule: &[SchedEvent]| -> TrialOutcome {
            let ai = schedule.iter().position(|event| event == &a).unwrap();
            let bi = schedule.iter().position(|event| event == &b).unwrap();
            Ok((ai < bi, schedule.to_vec()))
        };
        assert!(tester(&passing).unwrap().0);
        assert!(!tester(&failing).unwrap().0);
        assert!(tester(&[a.clone(), b.clone()]).unwrap().0);
        let mut calls = Vec::new();
        let error = search_for_critical_schedule(
            |schedule: &[SchedEvent]| {
                calls.push(schedule.to_vec());
                tester(schedule)
            },
            passing.clone(),
            failing.clone(),
            &Config::default(),
        )
        .unwrap_err();
        assert_eq!(calls, [passing, failing]);
        let no_swap = error
            .downcast_ref::<NoSingleSwap>()
            .unwrap_or_else(|| panic!("not NoSingleSwap: {error:#}"));
        assert_eq!((no_swap.unshared_events, no_swap.swaps_left), (1, 1));
        assert!(!no_swap.trimmed_failing_passed);
        let message = error.to_string();
        assert!(message.contains("the search stopped one swap"), "{message}");
        assert!(
            message.contains("has not established an exact one-swap pair"),
            "{message}"
        );
        assert!(message.contains("--prefix-search"), "{message}");
        assert!(!message.contains("The failure depends on"), "{message}");
        assert!(
            !message.contains("no single adjacent swap separates"),
            "{message}"
        );
        assert!(
            !message.contains("without the events only it has no longer fails"),
            "{message}"
        );
    }

    /// Schedules that run their shared events in the same order, and differ
    /// only in events one of them lacks, end the search with the same typed
    /// error, whose fallback is the prefix search, once the two endpoints are
    /// replayed. Before, the search stopped with an untyped "no remaining
    /// reorderable event distance" error, and analyze ended there.
    #[test]
    fn schedules_without_a_reordering_end_with_no_single_swap() {
        let a = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let b = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let c = SchedEvent::syscall(DetTid::from_raw(3), Sysno::getpid, SyscallPhase::Prehook);
        let extra = SchedEvent::syscall(DetTid::from_raw(7), Sysno::close, SyscallPhase::Prehook);
        let passing = vec![a.clone(), b.clone(), c.clone()];
        let failing = vec![a, b, extra.clone(), c];
        let tester = |schedule: &[SchedEvent]| -> TrialOutcome {
            Ok((!schedule.contains(&extra), schedule.to_vec()))
        };
        let error =
            search_for_critical_schedule(tester, passing, failing, &Config::default()).unwrap_err();
        let no_swap = error
            .downcast_ref::<NoSingleSwap>()
            .unwrap_or_else(|| panic!("not NoSingleSwap: {error:#}"));
        assert_eq!((no_swap.unshared_events, no_swap.swaps_left), (1, 0));
        let message = error.to_string();
        assert!(
            message.contains("run the events they share in the same order")
                && message.contains("--prefix-search"),
            "{message}"
        );
        assert!(
            message.contains("has not established an exact one-swap pair"),
            "{message}"
        );
        assert!(!message.contains("the failure depends on"), "{message}");
        assert!(
            !message.contains("no reordering of events separates"),
            "{message}"
        );

        // The same schedule with opposite outcomes is not a schedule
        // difference at all: no prefix search, an error naming the cause.
        let schedule = vec![
            SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook),
            SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook),
        ];
        let mut replays = 0;
        let flaky = |schedule: &[SchedEvent]| -> TrialOutcome {
            replays += 1;
            Ok((replays % 2 == 1, schedule.to_vec()))
        };
        let error =
            search_for_critical_schedule(flaky, schedule.clone(), schedule, &Config::default())
                .unwrap_err();
        assert!(error.downcast_ref::<NoSingleSwap>().is_none(), "{error:#}");
        assert!(
            error.to_string().contains("same sequence of events"),
            "{error:#}"
        );
    }

    /// A control schedule whose replays never agree ends the search with an
    /// error, not a panic.
    #[test]
    fn an_unstable_control_schedule_is_an_error() {
        let a = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let mut trial = 0;
        // Every replay realizes a trace no other replay does.
        let mut tester = |schedule: &[SchedEvent]| -> TrialOutcome {
            trial += 1;
            let realized = std::iter::repeat_n(a.clone(), trial)
                .chain(schedule.iter().cloned())
                .collect();
            Ok((true, realized))
        };
        let cfg = Config {
            max_jitter_editdist: 0,
            max_jitter_swapdist: 0,
            ..Default::default()
        };
        let requested = vec![a.clone()];
        let error = test_and_select_stable_point(&mut tester, &requested, &cfg).unwrap_err();
        assert!(error.to_string().contains("was not stable"), "{error}");
        assert_eq!(trial, cfg.max_replay_attempts);
    }

    /// Two agreeing replays classify a control schedule even when the first
    /// replay was the odd one out.
    #[test]
    fn two_agreeing_replays_classify_a_schedule_after_an_outlier() {
        let a = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let b = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let mut trial = 0;
        let mut tester = |_: &[SchedEvent]| -> TrialOutcome {
            trial += 1;
            // The first replay fails in another order; the next two agree.
            Ok(if trial == 1 {
                (false, vec![b.clone(), a.clone()])
            } else {
                (true, vec![a.clone(), b.clone(), b.clone()])
            })
        };
        let cfg = Config {
            max_jitter_editdist: 0,
            max_jitter_swapdist: 0,
            ..Default::default()
        };
        let requested = vec![a.clone(), b.clone()];
        let (passes, selected) =
            test_and_select_stable_point(&mut tester, &requested, &cfg).unwrap();
        assert!(passes, "the agreeing replays' outcome");
        assert_eq!(selected, requested);
        assert_eq!(trial, 3);
    }

    #[test]
    fn replays_differing_only_by_branch_skid_are_one_interleaving() {
        let tid = DetTid::from_raw(3);
        let other = DetTid::from_raw(5);
        let trace = |count: u32, end_time: u64| {
            vec![
                SchedEvent {
                    end_time: Some(detcore::types::LogicalTime::from_nanos(end_time)),
                    ..SchedEvent::branches(tid, count)
                },
                SchedEvent::syscall(other, Sysno::read, SyscallPhase::Prehook),
            ]
        };
        assert!(
            same_interleaving(
                &trace(115, 10),
                &trace(121, 70),
                INEXACT_COUNTER_BRANCH_JITTER
            ),
            "a few branches of skid and the times it shifts are the same interleaving"
        );
        assert!(
            !same_interleaving(&trace(115, 10), &trace(500, 10), 64),
            "a preemption hundreds of branches away is a different trace"
        );
        let mut reordered = trace(115, 10);
        reordered.reverse();
        assert!(
            !same_interleaving(&trace(115, 10), &reordered, 64),
            "another order is another interleaving"
        );
    }

    /// Two replays of one control schedule count as the same realized trace
    /// only when they are identical, event times included, unless the host's
    /// branch counter is not exact: then a few branches of skid, and the times
    /// they shift, are accepted, and by how much is said.
    #[test]
    fn replays_must_be_identical_unless_the_branch_counter_is_inexact() {
        let tid = DetTid::from_raw(3);
        let other = DetTid::from_raw(5);
        let trace = |count: u32, end_time: u64| {
            vec![
                SchedEvent {
                    end_time: Some(LogicalTime::from_nanos(end_time)),
                    ..SchedEvent::branches(tid, count)
                },
                SchedEvent::syscall(other, Sysno::read, SyscallPhase::Prehook),
            ]
        };
        let inexact = Some(INEXACT_COUNTER_BRANCH_JITTER);
        for jitter in [None, inexact] {
            assert_eq!(
                replay_agreement(&trace(115, 10), &trace(115, 10), jitter),
                ReplayAgreement::Identical
            );
            assert_eq!(
                replay_agreement(&trace(115, 10), &trace(500, 10), jitter),
                ReplayAgreement::Different,
                "a preemption hundreds of branches away is another trace"
            );
        }
        // An exact counter: six branches apart, or only a later end time, is
        // replay nondeterminism.
        assert_eq!(
            replay_agreement(&trace(115, 10), &trace(121, 70), None),
            ReplayAgreement::Different
        );
        assert_eq!(
            replay_agreement(&trace(115, 10), &trace(115, 70), None),
            ReplayAgreement::Different
        );
        // An inexact one: accepted, with how far apart they are.
        assert_eq!(
            replay_agreement(&trace(115, 10), &trace(121, 70), inexact),
            ReplayAgreement::Within(ReplayDifference {
                max_branches: 6,
                max_end_time_ns: 60
            })
        );
        let said = accepted_replay_difference(
            2,
            ReplayDifference {
                max_branches: 6,
                max_end_time_ns: 60,
            },
            INEXACT_COUNTER_BRANCH_JITTER,
        );
        assert!(
            said.contains("not the same realized trace: branch counts differ by up to 6 and event end times by up to 60 ns")
                && said.contains("up to 64 branches allowed"),
            "{said}"
        );
    }

    /// On a host whose branch counter is exact (the default), a jittered
    /// control schedule whose two replays differ by a few branches is not
    /// stable, and no outcome is attached to it; with the inexact counter's
    /// tolerance the same replays are accepted.
    #[test]
    fn a_skidded_replay_is_unstable_on_an_exact_counter() {
        let tid = DetTid::from_raw(3);
        let requested = vec![
            SchedEvent::branches(tid, 100),
            SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook),
        ];
        let replays = |calls: &mut u32| -> TrialOutcome {
            *calls += 1;
            // Each replay ends its slice a few branches from the request, and
            // from the other replay.
            let mut actual = requested.clone();
            actual[0].count = 100 + 3 * *calls;
            Ok((true, actual))
        };
        let mut calls = 0;
        let inexact = Config {
            max_branch_count_jitter: Some(INEXACT_COUNTER_BRANCH_JITTER),
            ..Default::default()
        };
        let (passes, selected) = test_and_select_stable_point(
            &mut |_: &[SchedEvent]| replays(&mut calls),
            &requested,
            &inexact,
        )
        .unwrap();
        assert!(passes);
        assert_eq!(selected, requested);
        let mut calls = 0;
        let unstable = test_and_select_stable_point(
            &mut |_: &[SchedEvent]| replays(&mut calls),
            &requested,
            &Config::default(),
        );
        assert!(
            unstable.is_err_and(|error| error.to_string().contains("not identical")),
            "an exact counter must not accept skidded replays"
        );
    }

    #[test]
    fn oversized_needleman_alignment_falls_back() {
        let first = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let second = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let third = SchedEvent::syscall(DetTid::from_raw(7), Sysno::getpid, SyscallPhase::Prehook);
        let passing = vec![first.clone(), second.clone(), third.clone()];
        let failing = vec![third, second, first.clone()];
        let tester = |schedule: &[SchedEvent]| -> TrialOutcome {
            Ok((schedule.first() == Some(&first), schedule.to_vec()))
        };
        let cfg = Config {
            max_needleman_matrix_cells: 0,
            needleman_search: true,
            refine_sub_events: false,
            ..Default::default()
        };

        let critical = search_for_critical_schedule(tester, passing, failing, &cfg).unwrap();

        assert_eq!(
            just_distance(&critical.passing_schedule, &critical.failing_schedule),
            (1, 1)
        );
    }

    /// The flaky_cas fixture endpoints, `(passing, failing)`, with the two
    /// critical events marked: `end_time` 0 on the first and 1 on the second.
    fn flaky_cas_endpoints() -> (Vec<SchedEvent>, Vec<SchedEvent>) {
        let passing_preemptions: PreemptionRecord = serde_json::from_slice(include_bytes!(
            "../../../test-resources/flaky_cas_sequence_schedules-passing.json"
        ))
        .expect("Failed to parse passing schedule");
        let failing_preemptions: PreemptionRecord = serde_json::from_slice(include_bytes!(
            "../../../test-resources/flaky_cas_sequence_schedules-failing.json"
        ))
        .expect("Failed to parse failing schedule");

        let mut passing_sched = passing_preemptions.into_global();
        let mut failing_sched = failing_preemptions.into_global();

        let critical_event_1_index_in_passing_schedule = 385;
        let critical_event_2_index_in_passing_schedule = 401;

        let critical_event_1_index_in_failing_schedule = 400;
        let critical_event_2_index_in_failing_schedule = 379;

        passing_sched.iter_mut().enumerate().for_each(|(i, e)| {
            e.end_time = if i == critical_event_1_index_in_passing_schedule {
                Some(LogicalTime::from_nanos(0))
            } else if i == critical_event_2_index_in_passing_schedule {
                Some(LogicalTime::from_nanos(1))
            } else {
                None
            }
        });

        failing_sched.iter_mut().enumerate().for_each(|(i, e)| {
            e.end_time = if i == critical_event_1_index_in_failing_schedule {
                Some(LogicalTime::from_nanos(0))
            } else if i == critical_event_2_index_in_failing_schedule {
                Some(LogicalTime::from_nanos(1))
            } else {
                None
            }
        });

        (passing_sched, failing_sched)
    }

    /// The mocked run's outcome: a schedule passes when the first marked event
    /// it reaches is the first critical event.
    fn flaky_cas_passes(sched: &[SchedEvent]) -> bool {
        let criticals = sched
            .iter()
            .filter(|e| e.end_time.is_some())
            .collect::<Vec<_>>();

        criticals[0].end_time.unwrap().as_nanos() == 0
    }

    #[test]
    /// This test runs a real search but with mocked out actual hermit runs.
    fn flaky_cas_sequence_schedules() {
        let (passing_sched, failing_sched) = flaky_cas_endpoints();

        let mock_tester = |sched: &[SchedEvent]| -> TrialOutcome {
            Ok((flaky_cas_passes(sched), sched.to_owned()))
        };

        let CriticalSchedule {
            failing_schedule: critical_failing_schedule,
            critical_event_index,
            ..
        } = search_for_critical_schedule(
            mock_tester,
            passing_sched,
            failing_sched,
            &Default::default(),
        )
        .unwrap();

        assert_eq!(critical_event_index, 379);

        let thread_5_syscall = &critical_failing_schedule[critical_event_index];
        let thread_7_one_branch = &critical_failing_schedule[critical_event_index - 1];
        let thread_7_other_branches = &critical_failing_schedule[critical_event_index + 1];

        assert_eq!(thread_5_syscall.dettid.as_raw(), 5);
        assert_eq!(
            thread_5_syscall.op,
            Op::Syscall(Sysno::futex, SyscallPhase::Posthook),
        );

        assert_eq!(thread_7_one_branch.dettid.as_raw(), 7);
        assert_eq!(thread_7_one_branch.op, Op::Branch,);
        assert_eq!(thread_7_one_branch.count, 1);

        assert_eq!(thread_7_other_branches.dettid.as_raw(), 7);
        assert_eq!(thread_7_other_branches.op, Op::Branch,);
        assert_eq!(thread_7_other_branches.count, 16);
    }

    /// A trial that `--max-log-bytes` ended, as `classify_container_result`
    /// reports it to the trial's caller.
    fn log_cap_trial_error() -> anyhow::Error {
        anyhow::Error::new(crate::app::container::LogCapExceeded)
    }

    /// The search over the flaky_cas fixture, with a trial that fails at every
    /// position in turn: the two endpoint checks, the event-level midpoints,
    /// and the sub-event binary searches and final checks. Each time the search
    /// stops at the failing trial and returns its error, still a
    /// `LogCapExceeded`. Before trials could fail, the analyze tester had to
    /// panic instead (`expect("New run to succeed")`).
    #[test]
    fn a_failing_trial_ends_the_schedule_search_with_its_error() {
        let (passing, failing) = flaky_cas_endpoints();
        let mut total_trials = 0;
        let critical = search_for_critical_schedule(
            |sched| {
                total_trials += 1;
                Ok((flaky_cas_passes(sched), sched.to_vec()))
            },
            passing.clone(),
            failing.clone(),
            &Config::default(),
        )
        .unwrap();
        // The sweep below covers the sub-event search only if this search
        // reached it, as flaky_cas_sequence_schedules shows it does.
        assert_eq!(critical.critical_event_index, 379);
        assert!(
            total_trials > 2,
            "the fixture search made only {total_trials} trials"
        );

        for failing_trial in 1..=total_trials {
            let mut trials = 0;
            let result = search_for_critical_schedule(
                |sched| {
                    trials += 1;
                    if trials == failing_trial {
                        return Err(log_cap_trial_error());
                    }
                    Ok((flaky_cas_passes(sched), sched.to_vec()))
                },
                passing.clone(),
                failing.clone(),
                &Config::default(),
            );
            let error = result.expect_err("a failed trial must end the search with an error");
            assert!(
                error
                    .downcast_ref::<crate::app::container::LogCapExceeded>()
                    .is_some(),
                "trial {failing_trial} of {total_trials}: the error lost its type: {error:#}"
            );
            assert_eq!(
                trials, failing_trial,
                "the search ran more trials after trial {failing_trial} failed"
            );
        }
    }

    /// The replay of a jittered control schedule is a trial too: when that
    /// replay fails, no further replay runs and its error is returned,
    /// still a `LogCapExceeded`. The flaky_cas search above never needs this
    /// replay, so this is the test that covers it.
    #[test]
    fn a_failing_jitter_replay_returns_its_error() {
        let first = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let second = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let requested = vec![first, second];
        let extra = SchedEvent::syscall(DetTid::from_raw(7), Sysno::getpid, SyscallPhase::Prehook);
        let mut calls = 0;
        let mut tester = |schedule: &[SchedEvent]| -> TrialOutcome {
            calls += 1;
            if calls == 2 {
                return Err(log_cap_trial_error());
            }
            let mut actual = schedule.to_vec();
            actual.push(extra.clone());
            Ok((true, actual))
        };

        let error = test_and_select_stable_point(&mut tester, &requested, &Config::default())
            .expect_err("a failed replay must be returned");

        assert!(
            error
                .downcast_ref::<crate::app::container::LogCapExceeded>()
                .is_some(),
            "{error:#}"
        );
        assert_eq!(calls, 2);
    }
}
