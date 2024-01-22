/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use nix::sys::signal::Signal;

use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;

/// Encapsulate the set of threads that are waiting for a specific time in the future.
///
/// It's possible (but unlikely) that multiple threads are waiting for the same
/// nanosecond, and this structure must break that symmetry.
#[derive(Debug, Clone, Default)]
pub struct TimedEvents {
    // Inner btreeset is *always* non-empty:
    map: BTreeMap<LogicalTime, BTreeSet<TimedEvent>>,

    // There is only one alarm allowed at a time per process, so we keep track of the current alarm
    // for each process and replace it if any other is inserted.
    alarm_times: BTreeMap<DetPid, LogicalTime>,
}

/// An event that occurs at a particular time in the execution, typically at an offset in the future.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TimedEvent {
    // An upcoming alarm, destined for particular pids, with a designated tid in that process (if it
    // still exists).
    AlarmEvt(DetPid, DetTid, Signal),

    /// A timed event on a particular thread (sleep, timeout, etc)
    ThreadEvt(DetTid),
}

impl fmt::Display for TimedEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TimedEvent::ThreadEvt(dt) => write!(f, "ThreadEvt({})", dt),
            TimedEvent::AlarmEvt(dp, dt, sig) => write!(f, "AlarmEvt({},{},{})", dp, dt, sig),
        }
    }
}

impl TimedEvents {
    pub fn insert(&mut self, ns: LogicalTime, dt: DetTid) {
        let set = self.map.entry(ns).or_default();
        if !set.insert(TimedEvent::ThreadEvt(dt)) {
            panic!(
                "TimedEvents::insert should not take a DetTid which is *already* in the set: {}",
                dt
            );
        }
    }

    // Return the last absolute alarm time for this pid, if any.
    pub fn insert_alarm(
        &mut self,
        ns: LogicalTime,
        dp: DetPid,
        dt: DetTid,
        sig: Signal,
    ) -> Option<LogicalTime> {
        let old = self.alarm_times.insert(dp, ns);
        self.clear_old_alarm(old);

        let set = self.map.entry(ns).or_default();
        let evt = TimedEvent::AlarmEvt(dp, dt, sig);
        if !set.insert(evt) {
            panic!(
                "TimedEvents::insert should not insert an alarm event which is *already* in the set: {}",
                evt
            );
        }
        old
    }

    fn clear_old_alarm(&mut self, old: Option<LogicalTime>) {
        if let Some(time) = old {
            let set = self
                .map
                .get_mut(&time)
                .expect("internal invariant broken, entry missing");

            // Could use a drain_filter here, but it is nightly only:
            let mut to_remove = None;
            for evt in set.iter() {
                if matches!(evt, TimedEvent::AlarmEvt(_, _, _)) {
                    assert!(to_remove.is_none());
                    to_remove = Some(*evt);
                }
            }
            if let Some(evt) = to_remove {
                assert!(set.remove(&evt));
            }
        }
    }

    // Return the time of any previous alarm on this process.
    pub fn remove_alarm(&mut self, dp: DetPid) -> Option<LogicalTime> {
        let old = self.alarm_times.remove(&dp);
        self.clear_old_alarm(old);
        old
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Return the next event if its target time of occurrence is before the supplied time.
    /// Being a "pop", this destructively removes the entry.
    pub fn pop_if_before(
        &mut self,
        current_time: LogicalTime,
    ) -> Option<(LogicalTime, TimedEvent)> {
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
    pub fn pop(&mut self) -> Option<(LogicalTime, TimedEvent)> {
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
            let removed = set.remove(&TimedEvent::ThreadEvt(dettid));
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
    pub fn iter(&self) -> impl Iterator<Item = (LogicalTime, TimedEvent)> + '_ {
        self.map
            .iter()
            .flat_map(|(key, set)| set.iter().map(|dtid| (*key, *dtid)))
    }
}
