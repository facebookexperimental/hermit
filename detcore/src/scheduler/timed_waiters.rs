// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::types::DetTid;
use crate::types::LogicalTime;

/// Encapsulate the set of threads that are waiting for a specific time in the future.
///
/// It's possible (but unlikely) that multiple threads are waiting for the same
/// nanosecond, and this structure must break that symmetry.
#[derive(Debug, Clone, Default)]
pub struct TimedEvents {
    // Inner set is always non-empty:
    map: BTreeMap<LogicalTime, BTreeSet<DetTid>>,
}

impl TimedEvents {
    pub fn insert(&mut self, ns: LogicalTime, dt: DetTid) {
        let set = self.map.entry(ns).or_insert_with(BTreeSet::new);
        if !set.insert(dt) {
            panic!(
                "TimedEvents::insert should not take a DetTid which is *already* in the set: {}",
                dt
            );
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Return the next event if its target time of occurrence is before the supplied time.
    /// Being a "pop", this destructively removes the entry.
    pub fn pop_if_before(&mut self, current_time: LogicalTime) -> Option<(LogicalTime, DetTid)> {
        if let Some(mut entry) = self.map.first_entry() {
            let time_ns = *entry.key();
            if time_ns <= current_time {
                let set = entry.get_mut();
                let dettid = set.pop_first().expect("inner set cannot be empty");
                if set.is_empty() {
                    entry.remove();
                }
                Some((time_ns, dettid))
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Pop the next event unconditionally, if available.
    pub fn pop(&mut self) -> Option<(LogicalTime, DetTid)> {
        self.pop_if_before(LogicalTime::MAX)
    }

    /// Are there no timed events waiting?
    pub fn is_empty(&self) -> bool {
        // Here we rely on the invariant that there are no entries with empty sets on the RHS:
        self.map.is_empty()
    }

    /// Remove a specific thread from the set of those waiting on time to elapse.
    pub fn remove(&mut self, dettid: DetTid) {
        let mut to_remove: Option<LogicalTime> = None;
        let mut already_removed = false;
        for (time_key, set) in self.map.iter_mut() {
            let removed = set.remove(&dettid);
            if removed {
                if already_removed {
                    panic!(
                        "invariant violation: multiple entries for dtid {} in TimedEvents",
                        dettid
                    );
                } else {
                    already_removed = true;
                }
            }
            // Cannot allow empty sets to remain:
            if set.is_empty() {
                to_remove = Some(*time_key);
            }
        }
        if let Some(time) = to_remove {
            let _ = self.map.remove(&time);
        }
    }

    /// Iterate over the entries in the TimedEvents collection
    pub fn iter(&self) -> impl Iterator<Item = (LogicalTime, DetTid)> + '_ {
        self.map
            .iter()
            .map(|(key, set)| set.iter().map(|dtid| (*key, *dtid)))
            .flatten()
    }
}
