/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ops::Range;

use colored::Colorize;
use detcore::types::SchedEvent;
use edit_distance::generate_permutation;
use edit_distance::iterable_bubble_sort;

const MAX_EVENT_LEVEL_SEARCH_PASSES: usize = 100;

const MAX_JITTER_EDITDIST: usize = 0;
const MAX_JITTER_SWAPDIST: usize = 0;

const MAX_UNMATCHED_TOPRINT: usize = 10;

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

#[allow(dead_code)]
/// Search for a schedule which is failing the criteria, but is edit distance one from succeeding.
pub fn search_for_critical_schedule<F>(
    mut tester: F,
    initial_passing_schedule: Vec<SchedEvent>,
    initial_failing_schedule: Vec<SchedEvent>,
    verbose: bool,
) -> CriticalSchedule
where
    F: FnMut(&[SchedEvent]) -> (bool, Vec<SchedEvent>),
{
    eprintln!("Verifying pass/fail endpoints of the search, using schedule-trace replay:");
    assert!(tester(&initial_passing_schedule).0);
    assert!(!tester(&initial_failing_schedule).0);

    // Do the first level search at the event level
    let EventLevelSearchResult {
        passing_schedule,
        failing_schedule,
    } = event_level_search(
        &mut tester,
        initial_passing_schedule,
        initial_failing_schedule,
        verbose,
    );

    // TODO: reenable subdividing blocks of event and searching down to unit granularity:
    // sub_event_search(&mut tester, passing_schedule, failing_schedule)
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
    CriticalSchedule {
        failing_schedule,
        passing_schedule,
        critical_event_index,
    }
}

// Return edit distance and swap distance.
fn just_distance(sched1: &[SchedEvent], sched2: &[SchedEvent]) -> (usize, usize) {
    let bubbles = iterable_bubble_sort(sched1, sched2);
    (bubbles.edit_distance(), bubbles.swap_distance())
}

/// Perform a multi-level search of the schedule space to find the critical schedule
/// between the two starting schedules by evaluating the given function at each step.
/// The result will be a failing schedule and coordinates in that schedule for the
/// adjacent branches that when flipped will cause the test to fail
fn event_level_search<F>(
    tester: &mut F,
    mut passing_schedule: Vec<SchedEvent>,
    mut failing_schedule: Vec<SchedEvent>,
    verbose: bool,
) -> EventLevelSearchResult
where
    F: FnMut(&[SchedEvent]) -> (bool, Vec<SchedEvent>),
{
    for pass_number in 0..MAX_EVENT_LEVEL_SEARCH_PASSES {
        let (swap_dist, edit_dist, requested_midpoint_schedule) = {
            let mut bubbles = iterable_bubble_sort(&passing_schedule, &failing_schedule);
            (
                bubbles.swap_distance(),
                bubbles.edit_distance(),
                bubbles.midpoint().cloned().collect::<Vec<_>>(),
            )
        };
        eprintln!(
            ":: Event-Level Search Pass {} => EditDistance = {}, Swap Distance = {} ({:.0}% matched, midpoint sched len = {})",
            pass_number,
            edit_dist,
            swap_dist,
            (100.0 * 2.0 * requested_midpoint_schedule.len() as f64
                / (passing_schedule.len() + failing_schedule.len()) as f64),
            requested_midpoint_schedule.len()
        );

        if verbose {
            let unmatched_total = passing_schedule.len() + failing_schedule.len()
                - 2 * requested_midpoint_schedule.len();
            if unmatched_total <= MAX_UNMATCHED_TOPRINT {
                let perm = generate_permutation(&passing_schedule, &failing_schedule);
                for ix in perm.unmatched_source_indices {
                    eprintln!(" :: unmatched source evt: {}", passing_schedule[ix]);
                }
                for ix in perm.unmatched_target_indices {
                    eprintln!(" :: unmatched target evt: {}", failing_schedule[ix]);
                }
            }
        }

        if swap_dist == 0 {
            panic!(
                "Aborting search, if the swap distance is 0, that means we didn't find ANY matching events between schedules. This is a bad sign."
            );
        }

        if swap_dist == 1 {
            return EventLevelSearchResult {
                passing_schedule,
                failing_schedule,
            };
        }

        let (midpoint_passes, midpoint_actual_schedule) = tester(&requested_midpoint_schedule);

        let (jitter_edit, jitter_swap) =
            just_distance(&requested_midpoint_schedule, &midpoint_actual_schedule);
        if verbose {
            if jitter_edit > 0 || jitter_swap > 0 {
                eprintln!(
                    ":: Jitter was {},{} edit/swap distance (requested synthetic schedule vs actual schedule)",
                    jitter_edit, jitter_swap
                );
            } else {
                eprintln!(
                    ":: No jitter in this run (requested synthetic schedule identical to actual schedule)",
                );
            }
        }

        let selected_new_point = if jitter_edit > MAX_JITTER_EDITDIST
            || jitter_swap > MAX_JITTER_SWAPDIST
        {
            eprintln!(
                ":: {} ({},{})",
                "Jitter exceeded threshold, proceeding optimistically along original route rather than rerouting the search".red().bold(),
                jitter_edit,
                jitter_swap,
            );
            requested_midpoint_schedule
        } else {
            midpoint_actual_schedule
        };

        if midpoint_passes {
            passing_schedule = selected_new_point;
        } else {
            failing_schedule = selected_new_point;
        }
    }

    panic!(
        "Event-Level Search Failed - No convergence after {} passes",
        MAX_EVENT_LEVEL_SEARCH_PASSES
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
///
#[allow(unused)]
fn sub_event_search<F>(
    tester: &mut F,
    passing_schedule: Vec<SchedEvent>,
    failing_schedule: Vec<SchedEvent>,
) -> CriticalSchedule
where
    F: FnMut(&[SchedEvent]) -> (bool, Vec<SchedEvent>),
{
    let (prefix, postfix) = get_common_pre_and_postfix(&passing_schedule, &failing_schedule);
    let critical_range = Range {
        start: prefix.len(),
        end: prefix.len() + 2,
    };

    eprintln!(
        "Passign Size - {}, Failing Size - {}, Prefix Size - {}, Postfix Size - {}",
        passing_schedule.len(),
        failing_schedule.len(),
        prefix.len(),
        postfix.len(),
    );

    let passing_critical_events = &passing_schedule[critical_range.clone()];
    let failing_critical_events = &failing_schedule[critical_range];

    // Ideally, the two critical pairs have the same count, but nothing guarantees that,
    // so we are taking the maximum count here. The rationalle for using the max is during
    // execution, if we specify too few in the count, the program could leave some scheduled
    // branches unevaluated, but given too many, the program cannot execute more branches than
    // it has, so it will likely desync and then reaquire later on.
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

    // Once this search is done, i_plus_one will be the minimum number of
    // branches from the first event in the critical pair where the test passes.
    // Thereform i will the maximum value where the test fails
    let i_plus_one = binary_search(0..i_max, &mut |i| {
        let schedule =
            create_schedule_with_critical_pair(prefix, postfix, &critical_pair_passing, i, j_max);

        let (passed, _) = tester(&schedule);

        eprintln!("Binary Search of A at sample {} -> passed = {}", i, passed);

        !passed
    });

    let i = i_plus_one - 1;

    // Once this search is done, j will be the minimum number of branches
    // from the second event in the critical pair to cause the test to fail.
    // Therefor j - 1 is the maximum number of branches to allow the test
    // to pass
    let j = binary_search(1..j_max, &mut |j| {
        let schedule =
            create_schedule_with_critical_pair(prefix, postfix, &critical_pair_passing, i, j);

        let (passed, _) = tester(&schedule);
        eprintln!("Binary Search of B at sample {} -> passed = {}", j, passed);

        passed
    });

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
    CriticalSchedule {
        failing_schedule,
        passing_schedule,
        critical_event_index,
    }
}

/// Binary search implementation specific to our problem. This will return the smallest value
/// where the predicate returns false
fn binary_search<F>(range: Range<u32>, predicate: &mut F) -> u32
where
    F: FnMut(u32) -> bool,
{
    let mut left = range.start;
    let mut right = range.end;
    let mut size = right - left;

    while size > 0 {
        let mid = left + size / 2;
        let test_res = predicate(mid);

        if test_res {
            left = mid + 1;
        } else {
            right = mid;
        }

        size = right - left;
    }

    left
}

/// Create a new SchedEvent with the size replaced with the given value
fn sched_event_with_new_count(original: &SchedEvent, new_count: u32) -> SchedEvent {
    SchedEvent {
        count: new_count,
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
///
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
        sched_event_with_new_count(&critical_pair_passing[0], i),
        sched_event_with_new_count(&critical_pair_passing[1], j),
        sched_event_with_new_count(&critical_pair_passing[0], i_remainder),
        sched_event_with_new_count(&critical_pair_passing[1], j_remainder),
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
        assert_eq!(binary_search(0..10, &mut |i| i < 4), 4);
        assert_eq!(binary_search(0..11, &mut |i| i < 4), 4);
        assert_eq!(binary_search(0..10, &mut |i| i < 5), 5);
        assert_eq!(binary_search(0..11, &mut |i| i < 5), 5);
        assert_eq!(binary_search(1..100, &mut |i| i < 49), 49);
        assert_eq!(binary_search(2..151, &mut |i| i < 49), 49);
        assert_eq!(binary_search(3..100, &mut |i| i < 58), 58);
        assert_eq!(binary_search(4..151, &mut |i| i < 58), 58);
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
        } = search_for_critical_schedule(mock_tester, passing_sched, failing_sched, true);

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
        // TODO: restore when sub_event_search is restored:
        // assert_eq!(thread_7_one_branch.count, 1);

        // assert_eq!(thread_7_other_branches.dettid.as_raw(), 7);
        assert_eq!(thread_7_other_branches.op, Op::Branch,);
        // assert_eq!(thread_7_other_branches.count, 16);
    }
}
