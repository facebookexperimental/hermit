/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Summaries of complete hermit runs.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

use crate::pid::DetTid;
use crate::time::LogicalTime;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#252): Confirm the overflow and rounding policy for long-running guests.
/// Running distribution statistics over timeslice durations, measured in virtual
/// nanoseconds. A "timeslice" is the span of virtual time a thread runs between
/// two consecutive scheduler yields (i.e. between `end_of_timeslice` resets).
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimesliceStats {
    /// Number of completed timeslices recorded.
    pub count: u64,
    /// Sum of all timeslice durations, in virtual nanoseconds.
    pub sum_ns: u64,
    /// Smallest timeslice duration observed, in virtual nanoseconds (valid when `count > 0`).
    pub min_ns: u64,
    /// Largest timeslice duration observed, in virtual nanoseconds (valid when `count > 0`).
    pub max_ns: u64,
}

impl TimesliceStats {
    /// Record one completed timeslice of `ns` virtual nanoseconds.
    pub fn record(&mut self, ns: u64) {
        if self.count == 0 {
            self.min_ns = ns;
            self.max_ns = ns;
        } else {
            self.min_ns = self.min_ns.min(ns);
            self.max_ns = self.max_ns.max(ns);
        }
        self.sum_ns += ns;
        self.count += 1;
    }

    /// Fold another distribution into this one.
    pub fn merge(&mut self, other: &TimesliceStats) {
        if other.count == 0 {
            return;
        }
        if self.count == 0 {
            *self = *other;
            return;
        }
        self.min_ns = self.min_ns.min(other.min_ns);
        self.max_ns = self.max_ns.max(other.max_ns);
        self.sum_ns += other.sum_ns;
        self.count += other.count;
    }

    /// Mean timeslice duration in virtual nanoseconds (0 when no slices recorded).
    pub fn mean_ns(&self) -> u64 {
        self.sum_ns.checked_div(self.count).unwrap_or(0)
    }

    /// Whether any timeslices have been recorded.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Statistics that summarize a hermit run.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct RunSummary {
    /// Internal number of steps taken by the scheduler.
    pub sched_turns: u64,

    /// **Trace replay:** SchedEvents read and replayed from the input recording.
    pub schedevent_replayed: u64,
    /// **Trace replay:** SchedEvents recorded to disk during execution.
    pub schedevent_recorded: u64,
    /// **Trace replay:** Desync events that occurred while replaying SchedEvents.
    pub schedevent_desynced: u64,

    /// A human-readable summary of the desyncs that occurred.
    pub desync_descrip: Option<String>,

    /// A summary of when threads where preempted and reprioritized (for --chaos mode), e.g. --record-preemptions-to.
    pub reprio_descrip: Option<String>,

    /// A summary of the thread topology spawned by the guest.
    pub threads_descrip: String,

    /// The number of threads that were group leaders, i.e. processes.
    pub num_processes: u64,
    /// The number of total system threads that were created during the execution.
    pub num_threads: u64,

    /// Deterministic virtual nanoseconds elapsed while computing.
    pub virttime_elapsed: u64,
    /// Absolute (virtual) time in nanoseconds since epoch at program completion.
    pub virttime_final: u64,

    /// **Nondeterministic:** Realtime in nanoseconds, i.e. wall-clock time elapsed.
    pub realtime_elapsed: Option<Duration>,

    /// Aggregate distribution of scheduler timeslice durations (virtual ns),
    /// summed over all threads.
    pub timeslice_stats: TimesliceStats,

    /// Per-thread timeslice distributions, sorted by `DetTid` for deterministic
    /// output.
    pub per_thread_timeslice: Vec<(DetTid, TimesliceStats)>,
}

/// A human-readable, multi-line summary.
impl fmt::Display for RunSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let RunSummary {
            sched_turns,
            schedevent_replayed,
            schedevent_recorded,
            schedevent_desynced,
            desync_descrip,
            reprio_descrip,
            num_processes,
            num_threads,
            virttime_elapsed,
            virttime_final,
            realtime_elapsed,
            threads_descrip,
            timeslice_stats,
            per_thread_timeslice,
        } = self;
        writeln!(f, "Final thread-tree was: {}", threads_descrip)?;
        writeln!(
            f,
            "There were {} group leaders of {} thread(s) total.",
            num_processes, num_threads
        )?;
        writeln!(
            f,
            "Internally, the hermit scheduler ran {} turns, recorded {} events, replayed {} events ({} desynced)",
            sched_turns, schedevent_recorded, schedevent_replayed, schedevent_desynced,
        )?;

        if let Some(txt) = desync_descrip {
            write!(f, "{}", txt)?;
        }
        if let Some(txt) = reprio_descrip {
            write!(f, "{}", txt)?;
        }

        writeln!(
            f,
            "Final virtual global (cpu) time: {}",
            LogicalTime::from_nanos(*virttime_final)
        )?;
        writeln!(
            f,
            "Elapsed virtual global (cpu) time: {}",
            LogicalTime::from_nanos(*virttime_elapsed)
        )?;

        if timeslice_stats.is_empty() {
            writeln!(f, "Timeslice stats: none recorded")?;
        } else {
            writeln!(
                f,
                "Timeslice stats: min={}ns max={}ns mean={}ns count={}",
                timeslice_stats.min_ns,
                timeslice_stats.max_ns,
                timeslice_stats.mean_ns(),
                timeslice_stats.count,
            )?;
            // Per-thread breakdown (only informative when more than one thread
            // recorded slices); shown as part of the report body.
            if per_thread_timeslice.len() > 1 {
                for (dettid, st) in per_thread_timeslice {
                    if st.is_empty() {
                        continue;
                    }
                    writeln!(
                        f,
                        "  timeslice thread {}: min={}ns max={}ns mean={}ns count={}",
                        dettid,
                        st.min_ns,
                        st.max_ns,
                        st.mean_ns(),
                        st.count,
                    )?;
                }
            }
        }

        if let Some(rt) = realtime_elapsed {
            writeln!(f, "Nondeterministic realtime elapsed: {:?}", rt)?
        };

        Ok(())
    }
}

/*
  ------------------------------ hermit run report ------------------------------
Final thread-tree was: [3]
There were 1 group leaders of 1 thread(s) total.
Internally, the hermit scheduler ran 8 turns, recorded 0 events, replayed 0 events (0 desynced)
Nondeterministic realtime elapsed: 27.08914ms
Final virtual global (cpu) time: 1_640_995_199.005_045_040s
Elapsed virtual global (cpu) time: 5_045_040ns
Timeslice stats: min=199999995ns max=200000000ns mean=199999998ns count=4
*/

#[cfg(test)]
mod tests {
    use super::TimesliceStats;

    #[test]
    fn timeslice_stats_empty() {
        let s = TimesliceStats::default();
        assert!(s.is_empty());
        assert_eq!(s.count, 0);
        assert_eq!(s.mean_ns(), 0); // no divide-by-zero
    }

    #[test]
    fn timeslice_stats_record() {
        let mut s = TimesliceStats::default();
        s.record(10);
        s.record(30);
        s.record(20);
        assert!(!s.is_empty());
        assert_eq!(s.count, 3);
        assert_eq!(s.min_ns, 10);
        assert_eq!(s.max_ns, 30);
        assert_eq!(s.sum_ns, 60);
        assert_eq!(s.mean_ns(), 20);
    }

    #[test]
    fn timeslice_stats_merge() {
        let mut a = TimesliceStats::default();
        a.record(10);
        a.record(40);
        let mut b = TimesliceStats::default();
        b.record(5);
        b.record(100);
        a.merge(&b);
        assert_eq!(a.count, 4);
        assert_eq!(a.min_ns, 5);
        assert_eq!(a.max_ns, 100);
        assert_eq!(a.sum_ns, 155);

        // Merging an empty distribution is a no-op.
        let before = a;
        a.merge(&TimesliceStats::default());
        assert_eq!(a, before);

        // Merging into an empty distribution adopts the other.
        let mut empty = TimesliceStats::default();
        empty.merge(&b);
        assert_eq!(empty, b);
    }
}
