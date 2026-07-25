/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ops::Range;

use anyhow::bail;
use colored::Colorize;
use detcore::types::SchedEvent;
use edit_distance::NeedlemanWunsch;
use edit_distance::NeedlemanWunschError;
use edit_distance::generate_permutation;
use edit_distance::iterable_bubble_sort;

/// User configurable settings.
pub struct Config {
    pub max_jitter_editdist: usize,
    pub max_jitter_swapdist: usize,
    pub max_event_level_search_passes: usize,
    pub max_unmatched_to_print: usize,
    /// Bound quadratic traceback work before falling back to event permutation search.
    pub max_needleman_matrix_cells: usize,
    /// Number of identical replays required to classify a jittered control schedule.
    pub max_replay_attempts: usize,
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
            max_replay_attempts: 2,
            refine_sub_events: true,
            verbose: false,
            needleman_search: false,
        }
    }
}

struct EventLevelSearchResult {
    passing_schedule: Vec<SchedEvent>,
    failing_schedule: Vec<SchedEvent>,
}

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
    if (first.dettid == second.dettid)
        && (first.op == second.op)
        && (first.start_rip == second.start_rip)
        && (first.end_rip == second.end_rip)
    {
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
pub fn search_for_critical_schedule<F>(
    mut tester: F,
    initial_passing_schedule: Vec<SchedEvent>,
    initial_failing_schedule: Vec<SchedEvent>,
    cfg: &Config,
) -> anyhow::Result<CriticalSchedule>
where
    F: FnMut(&[SchedEvent]) -> (bool, Vec<SchedEvent>),
{
    eprintln!("Verifying pass/fail endpoints of the search, using schedule-trace replay:");
    let (passing_endpoint_passes, _) = tester(&initial_passing_schedule);
    if !passing_endpoint_passes {
        bail!("the passing schedule endpoint did not reproduce a passing outcome");
    }
    let (failing_endpoint_passes, _) = tester(&initial_failing_schedule);
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
        )
    } else {
        event_level_search(
            &mut tester,
            initial_passing_schedule,
            initial_failing_schedule,
            cfg,
        )
    };

    if cfg.refine_sub_events
        && let Some(critical_schedule) = sub_event_search(
            &mut tester,
            passing_schedule.clone(),
            failing_schedule.clone(),
        )
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
        let (common_prefix, _) = get_common_pre_and_postfix(&passing_schedule, &failing_schedule);
        common_prefix.len() + 1
    };
    eprintln!("Critical event index {}", critical_event_index);
    Ok(CriticalSchedule {
        failing_schedule,
        passing_schedule,
        critical_event_index,
    })
}

// Return edit distance and swap distance.
fn just_distance(sched1: &[SchedEvent], sched2: &[SchedEvent]) -> (usize, usize) {
    let bubbles = iterable_bubble_sort(sched1, sched2);
    (bubbles.edit_distance(), bubbles.swap_distance())
}

fn test_and_select_stable_point<F>(
    tester: &mut F,
    requested_schedule: &[SchedEvent],
    cfg: &Config,
) -> (bool, Vec<SchedEvent>)
where
    F: FnMut(&[SchedEvent]) -> (bool, Vec<SchedEvent>),
{
    let (passes, actual_schedule) = tester(requested_schedule);
    let jitter = just_distance(requested_schedule, &actual_schedule);
    if jitter.0 <= cfg.max_jitter_editdist && jitter.1 <= cfg.max_jitter_swapdist {
        return (passes, actual_schedule);
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

    for attempt in 2..=cfg.max_replay_attempts.max(2) {
        let (replayed_passes, replayed_actual) = tester(requested_schedule);
        if replayed_passes == passes && replayed_actual == actual_schedule {
            eprintln!(
                ":: Jittered control schedule reproduced the same outcome and realized trace on attempt {}",
                attempt,
            );
            return (passes, requested_schedule.to_vec());
        }
    }

    panic!(
        "Jittered schedule replay was not stable after {} attempts; refusing to attach an outcome to the requested control schedule",
        cfg.max_replay_attempts.max(2),
    );
}

/// Search of the schedule space to find the critical schedule
fn needleman_level_search<F>(
    tester: &mut F,
    mut passing_schedule: Vec<SchedEvent>,
    mut failing_schedule: Vec<SchedEvent>,
    cfg: &Config,
) -> EventLevelSearchResult
where
    F: FnMut(&[SchedEvent]) -> (bool, Vec<SchedEvent>),
{
    for pass_number in 0..cfg.max_event_level_search_passes {
        let (edit_distance, swap_distance) = just_distance(&passing_schedule, &failing_schedule);
        if edit_distance == 1 && swap_distance == 1 {
            return EventLevelSearchResult {
                passing_schedule,
                failing_schedule,
            };
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
            test_and_select_stable_point(tester, &requested_midpoint_schedule, cfg);

        if midpoint_passes {
            passing_schedule = selected_new_point;
        } else {
            failing_schedule = selected_new_point;
        }
    }

    panic!(
        "Event-Level Search Failed - No convergence after {} passes",
        cfg.max_event_level_search_passes
    );
}

/// Generate a permutation between source and target and print unmatched events (up to a limit).
fn print_unmatched_events(
    passing_schedule: &[SchedEvent],
    failing_schedule: &[SchedEvent],
    max_to_print: usize,
) {
    let perm = generate_permutation(passing_schedule, failing_schedule);
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
) -> EventLevelSearchResult
where
    F: FnMut(&[SchedEvent]) -> (bool, Vec<SchedEvent>),
{
    let orig_passing_schedule = passing_schedule.clone();
    let orig_failing_schedule = failing_schedule.clone();

    for pass_number in 0..cfg.max_event_level_search_passes {
        let (swap_dist, edit_dist, requested_midpoint_schedule) = {
            let mut bubbles = iterable_bubble_sort(&passing_schedule, &failing_schedule);
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

        if swap_dist == 0 {
            panic!(
                "Aborting search: opposite-outcome schedules have no remaining reorderable event distance (edit distance {})",
                edit_dist,
            );
        }

        if swap_dist == 1 && edit_dist == 1 {
            return EventLevelSearchResult {
                passing_schedule,
                failing_schedule,
            };
        }

        if swap_dist == 1 {
            panic!(
                "Aborting search: schedules are one swap apart but still contain {} unmatched event edits",
                edit_dist - swap_dist,
            );
        }

        let (midpoint_passes, selected_new_point) =
            test_and_select_stable_point(tester, &requested_midpoint_schedule, cfg);
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

    panic!(
        "Event-Level Search Failed - No convergence after {} passes",
        cfg.max_event_level_search_passes
    );
}

/// Returns `(prefix, postfix)` respectively.
fn get_common_pre_and_postfix<'a, T>(sched_1: &'a [T], sched_2: &[T]) -> (&'a [T], &'a [T])
where
    T: Eq,
{
    let common_prefix_len = sched_1
        .iter()
        .zip(sched_2.iter())
        .take_while(|(e1, e2)| e1 == e2)
        .count();

    let common_postfix_len = if sched_1.len() == sched_2.len() && sched_1.len() == common_prefix_len
    {
        0
    } else {
        sched_1
            .iter()
            .rev()
            .zip(sched_2.iter().rev())
            .take_while(|(e1, e2)| e1 == e2)
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
) -> Option<CriticalSchedule>
where
    F: FnMut(&[SchedEvent]) -> (bool, Vec<SchedEvent>),
{
    let (prefix, postfix) = get_common_pre_and_postfix(&passing_schedule, &failing_schedule);
    if passing_schedule.len() != failing_schedule.len()
        || prefix.len() + postfix.len() + 2 != passing_schedule.len()
    {
        eprintln!(
            ":: Skipping sub-event refinement: final schedules are not a single exact adjacent event reversal"
        );
        return None;
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
        return None;
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
        return None;
    }

    // Find the first A branch count that passes. Its predecessor is the failing boundary.
    let i_plus_one = binary_search(0..i_max, &mut |i| {
        let schedule =
            create_schedule_with_critical_pair(prefix, postfix, &critical_pair_passing, i, j_max);

        let passed = test_exact_replay(tester, &schedule)?;

        eprintln!("Binary Search of A at sample {} -> passed = {}", i, passed);

        Some(!passed)
    })?;

    let i = i_plus_one.checked_sub(1).or_else(|| {
        eprintln!(
            ":: Skipping sub-event refinement: A boundary is already passing at zero branches"
        );
        None
    })?;

    // Find the first B branch count that fails while holding A at its boundary.
    let j = binary_search(1..j_max, &mut |j| {
        let schedule =
            create_schedule_with_critical_pair(prefix, postfix, &critical_pair_passing, i, j);

        let passed = test_exact_replay(tester, &schedule)?;
        eprintln!("Binary Search of B at sample {} -> passed = {}", j, passed);

        Some(passed)
    })?;

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
        return None;
    }
    if !test_exact_replay(tester, &passing_schedule)?
        || test_exact_replay(tester, &failing_schedule)?
    {
        eprintln!(
            ":: Skipping sub-event refinement: final A/B replay did not preserve passing/failing outcomes"
        );
        return None;
    }

    eprintln!(
        ":: {}",
        "Critical branch boundary found and verified."
            .green()
            .bold()
    );
    Some(CriticalSchedule {
        failing_schedule,
        passing_schedule,
        critical_event_index,
    })
}

fn same_event_except_count(first: &SchedEvent, second: &SchedEvent) -> bool {
    let mut first = first.clone();
    let mut second = second.clone();
    first.count = 0;
    second.count = 0;
    first == second
}

fn can_split_event(event: &SchedEvent) -> bool {
    event.count > 0 && (event.op == detcore::types::Op::Branch || event.count == 1)
}

fn test_exact_replay<F>(tester: &mut F, requested: &[SchedEvent]) -> Option<bool>
where
    F: FnMut(&[SchedEvent]) -> (bool, Vec<SchedEvent>),
{
    let (passes, actual) = tester(requested);
    if actual != requested {
        eprintln!(
            ":: Skipping sub-event refinement: replay did not realize a requested branch boundary exactly (requested {} events, realized {})",
            requested.len(),
            actual.len(),
        );
        None
    } else {
        Some(passes)
    }
}

/// Binary search implementation specific to our problem. This will return the smallest value
/// where the predicate returns false
fn binary_search<F>(range: Range<u32>, predicate: &mut F) -> Option<u32>
where
    F: FnMut(u32) -> Option<bool>,
{
    let mut left = range.start;
    let mut right = range.end;
    let mut size = right - left;

    while size > 0 {
        let mid = left + size / 2;
        let test_res = predicate(mid)?;

        if test_res {
            left = mid + 1;
        } else {
            right = mid;
        }

        size = right - left;
    }

    Some(left)
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
    use detcore::types::DetTid;
    use detcore::types::LogicalTime;
    use detcore::types::Op;
    use detcore::types::SyscallPhase;
    use reverie::syscalls::Sysno;

    use super::*;

    #[test]
    fn test_common_pre_and_postfix() {
        assert_eq!(
            get_common_pre_and_postfix(b"abABcde", b"abBAcde"),
            (b"ab".as_slice(), b"cde".as_slice())
        );
    }

    #[test]
    fn test_binary_search() {
        // Check a bunch of close values to make sure we don't have an off-by-one problem.
        // I also did an exhaustive search up to 100,000 because I was sure this wouldn't work,
        // but it does. Thanks, Rust std library
        assert_eq!(binary_search(0..10, &mut |i| Some(i < 4)), Some(4));
        assert_eq!(binary_search(0..11, &mut |i| Some(i < 4)), Some(4));
        assert_eq!(binary_search(0..10, &mut |i| Some(i < 5)), Some(5));
        assert_eq!(binary_search(0..11, &mut |i| Some(i < 5)), Some(5));
        assert_eq!(binary_search(1..100, &mut |i| Some(i < 49)), Some(49));
        assert_eq!(binary_search(2..151, &mut |i| Some(i < 49)), Some(49));
        assert_eq!(binary_search(3..100, &mut |i| Some(i < 58)), Some(58));
        assert_eq!(binary_search(4..151, &mut |i| Some(i < 58)), Some(58));
        assert_eq!(binary_search(0..10, &mut |_| None), None);
    }

    #[test]
    fn syscall_pair_stays_at_event_granularity() {
        let first = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let second = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let passing = vec![first.clone(), second.clone()];
        let failing = vec![second, first.clone()];
        let tester =
            |schedule: &[SchedEvent]| (schedule.first() == Some(&first), schedule.to_vec());

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
            |schedule| (false, schedule.to_vec()),
            good.clone(),
            bad.clone(),
            &Config::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("passing schedule endpoint"));

        let error = search_for_critical_schedule(
            |schedule| (true, schedule.to_vec()),
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
        let mut tester = |schedule: &[SchedEvent]| {
            let mut actual = schedule.to_vec();
            actual.push(extra.clone());
            (false, actual)
        };

        assert!(sub_event_search(&mut tester, passing, failing).is_none());
    }

    #[test]
    fn jittered_control_point_requires_stable_replay() {
        let first = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let second = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let requested = vec![first, second];
        let extra = SchedEvent::syscall(DetTid::from_raw(7), Sysno::getpid, SyscallPhase::Prehook);
        let mut calls = 0;
        let mut tester = |schedule: &[SchedEvent]| {
            calls += 1;
            let mut actual = schedule.to_vec();
            actual.push(extra.clone());
            (true, actual)
        };

        let (passes, selected) =
            test_and_select_stable_point(&mut tester, &requested, &Config::default());

        assert!(passes);
        assert_eq!(selected, requested);
        assert_eq!(calls, 2);
    }

    #[test]
    fn oversized_needleman_alignment_falls_back() {
        let first = SchedEvent::syscall(DetTid::from_raw(3), Sysno::write, SyscallPhase::Prehook);
        let second = SchedEvent::syscall(DetTid::from_raw(5), Sysno::read, SyscallPhase::Prehook);
        let third = SchedEvent::syscall(DetTid::from_raw(7), Sysno::getpid, SyscallPhase::Prehook);
        let passing = vec![first.clone(), second.clone(), third.clone()];
        let failing = vec![third, second, first.clone()];
        let tester =
            |schedule: &[SchedEvent]| (schedule.first() == Some(&first), schedule.to_vec());
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

    #[test]
    /// This test runs a real search but with mocked out actual hermit runs.
    fn flaky_cas_sequence_schedules() {
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

        let mock_tester = |sched: &[SchedEvent]| {
            let criticals = sched
                .iter()
                .filter(|e| e.end_time.is_some())
                .collect::<Vec<_>>();

            (
                criticals[0].end_time.unwrap().as_nanos() == 0,
                sched.to_owned(),
            )
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
}
