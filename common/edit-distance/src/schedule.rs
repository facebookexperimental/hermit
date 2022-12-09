/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;

use serde::Deserialize;
use serde::Serialize;

#[allow(dead_code)]
pub type Schedule = Vec<MiniSchedEvent>;

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub enum Op {
    Branch,
    Rdtsc,
    Cpuid,
    Syscall(u32),
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct MiniSchedEvent {
    #[serde(alias = "dettid")]
    tid: u32,
    op: Op,
    count: usize,
    //start_rip: Option<usize>,
    //end_rip: Option<usize>,
    //end_time: u64,
}

/// Counts the number of events for each thread.
#[allow(dead_code)]
fn count_thread_events(schedule: &Schedule) -> HashMap<u32, usize> {
    let mut result = HashMap::new();

    for event in schedule {
        *result.entry(event.tid).or_insert(0) += event.count;
    }

    result
}

/// Counts the number of events for each thread.
#[allow(dead_code)]
fn partition_thread_events(schedule: Schedule) -> HashMap<u32, Schedule> {
    let mut result = HashMap::<u32, Vec<_>>::new();

    for event in schedule {
        result.entry(event.tid).or_default().push(event);
    }

    result
}

/// Prunes the passing schedule such that it is the same length as the failing
/// schedule. This is done by partitioning each schedule event by its thread ID.
#[allow(dead_code)]
pub fn prune_schedule(passing: Schedule, failing: &Schedule) -> Schedule {
    assert!(passing.len() >= failing.len());

    let mut result = Vec::with_capacity(failing.len());
    let mut failing_count = count_thread_events(failing);

    for event in passing {
        if let Some(count) = failing_count.get_mut(&event.tid) {
            if *count > 0 {
                result.push(event);
                *count = count.saturating_sub(event.count);
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::damerau_lev;

    // TODO: Find out if both schedules are permutations of each other (after
    // slicing off the end of the longer schedule).

    #[allow(dead_code)]
    static PASS: &str = include_str!("testdata/pass.json");
    #[allow(dead_code)]
    static FAIL: &str = include_str!("testdata/fail.json");

    // #[test]
    // TODO, unfinished:
    fn _smoke() {
        let passing_schedule: Schedule = serde_json::from_str(PASS).unwrap();
        assert_eq!(passing_schedule.len(), 239);
        let failing_schedule: Schedule = serde_json::from_str(FAIL).unwrap();
        assert_eq!(failing_schedule.len(), 237);

        let edit_dist = damerau_lev(&passing_schedule, &failing_schedule);
        assert_eq!(edit_dist, 26);

        let passing_schedule: HashSet<MiniSchedEvent> = HashSet::from_iter(passing_schedule);
        let failing_schedule: HashSet<MiniSchedEvent> = HashSet::from_iter(failing_schedule);

        let diff: Vec<_> = passing_schedule
            .symmetric_difference(&failing_schedule)
            .collect();

        assert_eq!(
            diff.len(),
            14,
            "Schedules are not permutations: {:#?}",
            diff
        );
    }

    // #[test]
    // TODO, unfinished:
    fn _pruning() {
        let passing: Schedule = serde_json::from_str(PASS).unwrap();
        let failing: Schedule = serde_json::from_str(FAIL).unwrap();

        let partitioned_passing = partition_thread_events(passing);
        let partitioned_failing = partition_thread_events(failing);

        assert_eq!(
            partitioned_passing[&5], partitioned_failing[&5],
            "passing: {:#?}\nfailing: {:#?}",
            partitioned_passing[&5], partitioned_failing[&5]
        );
    }
}
