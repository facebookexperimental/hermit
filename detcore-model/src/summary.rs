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

/// Per-execution SaBRe path evidence written as one JSON object per line.
///
/// The Hermit SaBRe runner produces this record at the path named by
/// `HERMIT_SABRE_PATH_EVIDENCE`; the manifest runner reads the same type before
/// deciding whether a SaBRe cell exercised the intended path. Keep the wire
/// fields here rather than independently defining the producer and reader
/// shapes: a missing or renamed counter changes whether a cell is accepted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PathEvidence {
    pub schema: u8,
    pub guest_rpc_observed: bool,
    pub ptrace_fallback_sites: usize,
    pub trusted_shared_object_sites: usize,
    pub trusted_shared_objects: Vec<String>,
}

impl PathEvidence {
    pub const SCHEMA: u8 = 1;
}

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

    /// Total syscalls completed by all guest threads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syscalls: Option<u64>,

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

/// A human-readable, multi-line summary. Serialized report fields are unchanged.
impl fmt::Display for RunSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_summary(f, true, self.reprio_descrip.as_deref())
    }
}

/// INFO view built with the producer's preemption description, before its host
/// destination is appended. It does not alter the full report or serialized fields.
pub struct RunSummaryInfo<'a> {
    summary: &'a RunSummary,
    reprio_description: Option<&'a str>,
}

impl fmt::Display for RunSummaryInfo<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.summary.fmt_summary(f, false, self.reprio_description)
    }
}

impl RunSummary {
    pub fn info<'a>(&'a self, reprio_description: Option<&'a str>) -> RunSummaryInfo<'a> {
        RunSummaryInfo {
            summary: self,
            reprio_description,
        }
    }

    fn fmt_summary(
        &self,
        f: &mut fmt::Formatter<'_>,
        include_replay_bookkeeping: bool,
        reprio_description: Option<&str>,
    ) -> fmt::Result {
        let RunSummary {
            sched_turns,
            schedevent_replayed,
            schedevent_recorded,
            schedevent_desynced,
            desync_descrip,
            reprio_descrip: _,
            num_processes,
            num_threads,
            syscalls,
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
        if let Some(syscalls) = syscalls {
            writeln!(f, "Guest threads completed {syscalls} syscalls.")?;
        }
        if include_replay_bookkeeping {
            writeln!(
                f,
                "Internally, the hermit scheduler ran {} turns, recorded {} events, replayed {} events ({} desynced)",
                sched_turns, schedevent_recorded, schedevent_replayed, schedevent_desynced,
            )?;
        } else {
            writeln!(
                f,
                "Internally, the hermit scheduler ran {} turns, recorded {} events ({} desynced)",
                sched_turns, schedevent_recorded, schedevent_desynced,
            )?;
        }

        if let Some(txt) = desync_descrip {
            write!(f, "{}", txt)?;
        }
        if let Some(txt) = reprio_description {
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
    use super::PathEvidence;
    use super::RunSummary;
    use super::TimesliceStats;

    #[test]
    fn path_evidence_json_is_exact() {
        let evidence = PathEvidence {
            schema: PathEvidence::SCHEMA,
            guest_rpc_observed: true,
            ptrace_fallback_sites: 0,
            trusted_shared_object_sites: 1,
            trusted_shared_objects: vec!["/usr/lib/libc.so.6".into()],
        };
        let json = serde_json::to_string(&evidence).unwrap();
        assert_eq!(
            json,
            r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":1,"trusted_shared_objects":["/usr/lib/libc.so.6"]}"#
        );
        assert_eq!(
            serde_json::from_str::<PathEvidence>(&json).unwrap(),
            evidence
        );

        let unknown = json.replace(
            r#""trusted_shared_objects""#,
            r#""unexpected":0,"trusted_shared_objects""#,
        );
        let error = serde_json::from_str::<PathEvidence>(&unknown).unwrap_err();
        assert!(error.to_string().contains("unknown field `unexpected`"));

        let missing = json.replace(r#""guest_rpc_observed":true,"#, "");
        let error = serde_json::from_str::<PathEvidence>(&missing).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing field `guest_rpc_observed`")
        );
    }

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

    #[test]
    fn older_summary_json_keeps_an_unrecorded_syscall_total_absent() {
        let value = serde_json::to_value(RunSummary::default()).unwrap();
        let mut object = value.as_object().unwrap().clone();
        object.remove("syscalls");
        let parsed: RunSummary = serde_json::from_value(object.into()).unwrap();
        assert_eq!(parsed.syscalls, None);

        let measured = RunSummary {
            syscalls: Some(0),
            ..Default::default()
        };
        let value = serde_json::to_value(&measured).unwrap();
        assert_eq!(value["syscalls"], 0);
        let parsed: RunSummary = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.syscalls, Some(0));
    }
}
