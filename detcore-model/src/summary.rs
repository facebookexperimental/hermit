/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Summaries of complete hermit runs.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// The backend dispatch record a run summary carries, re-exported so readers of
/// summaries and verification reports need no direct Reverie dependency.
pub use reverie::DispatchStats;
use serde::Deserialize;
use serde::Serialize;

use crate::host_input::HostInputChange;
use crate::host_input::HostInputSet;
use crate::pid::DetTid;
use crate::syscall_histogram::SyscallHistogramSummary;
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

/// Configured timeslice and the runtime's preemption timer mechanism.
///
/// This describes configuration and capability, not whether a hardware timer
/// is currently armed or any timer event was delivered.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct PreemptionSummary {
    /// Configured maximum in virtual nanoseconds, or `None` when disabled.
    pub max_timeslice_ns: Option<u64>,
    /// Whether the configured maximum has a runtime timer mechanism.
    /// `None` means an older producer did not record that fact.
    #[serde(default)]
    pub timer_enabled: Option<bool>,
    /// Timeslice ends processed without stopping the guest because no other
    /// thread could run (`Config::eager_lone_preemptions`), or `None` where
    /// that is off or does not apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopless_slice_ends: Option<u64>,
}

impl fmt::Display for PreemptionSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.max_timeslice_ns {
            Some(maximum) => write!(f, "Configured maximum timeslice: {maximum} virtual ns")?,
            None => write!(f, "Configured maximum timeslice: disabled")?,
        };
        let timer = match self.timer_enabled {
            Some(false) => "none",
            Some(true) => "enabled",
            None => "unknown",
        };
        write!(f, "; timer: {timer}")?;
        if let Some(ends) = self.stopless_slice_ends {
            write!(
                f,
                "; slice ends taken without a stop (no other thread could run): {ends}"
            )?;
        }
        Ok(())
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

    /// Configured maximum and timer mechanism from the shared Detcore config.
    /// Absent for older summaries or producers that did not record it; a present
    /// object with a null maximum explicitly records a disabled maximum. Timer
    /// availability is configuration/capability metadata, not measured delivery.
    /// Shown in the full report, but not the INFO guest-behavior view used by
    /// `--verify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preemption: Option<PreemptionSummary>,

    /// How the backend dispatched guest syscalls: through signals, patched
    /// direct calls, or ptrace stops. The record carries its own schema
    /// version. Absent when the run did not collect it or the backend has no
    /// such dispatch (KVM). Deliberately not part of the human-readable
    /// summary, whose INFO view is compared by `--verify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_stats: Option<DispatchStats>,

    /// Accepted findings that make this faithful preemption replay incomplete.
    /// Absent for ordinary runs and older JSON summaries; not part of Display
    /// or the INFO view compared by verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_departure: Option<crate::replay_departure::Summary>,

    /// Per-syscall entries and outcomes over all guest threads, with the
    /// scope of what was observed ([`crate::syscall_histogram`]); its `calls`
    /// sum to `syscalls` ([`RunSummary::syscall_histogram_is_consistent`]).
    /// Absent from summaries written before it existed. Not part of the
    /// human-readable summary, whose INFO view is compared by `--verify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syscall_histogram: Option<SyscallHistogramSummary>,
    /// The host files the guest opened, with the identity each path's first
    /// open found and the set's digest (see [`crate::host_input`]). Two
    /// separate runs whose digests differ read different host files, which
    /// Hermit does not make deterministic. Absent when the run did not
    /// record them. Not part of the human-readable summary, whose INFO view
    /// `--verify` compares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_inputs: Option<HostInputSet>,

    /// The host files a schedule replay found changed since the recording it
    /// replays, in the order the guest first opened them (see
    /// [`crate::host_input::HostInputChange`]). Empty when nothing changed or
    /// the run replayed nothing. Not part of the human-readable summary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_inputs_changed: Vec<HostInputChange>,
    /// The run ended on a deadlock under Hermit's model: every remaining thread
    /// was blocked with no wake Detcore models. Its status is then
    /// `HERMIT_GUEST_DEADLOCK_EXIT`, not one the guest exited with. Absent
    /// (false) for every other run and in older summaries.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub guest_deadlock: bool,

    /// The run ended because its virtual clock passed `--virtual-timeout`,
    /// whose bound in virtual nanoseconds this is: the scheduler granted no
    /// turn past it. Its status is then 124, not one the guest exited with.
    /// Absent for every other run and in older summaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_timeout_ns: Option<u64>,

    /// aarch64: the load/store exclusive pairs (LL/SC retry loops) in the
    /// guest's executable file mappings, by guest path. A function of the
    /// files' bytes, so it is in the INFO view. Empty elsewhere.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exclusive_pairs: BTreeMap<String, u64>,

    /// **Nondeterministic:** aarch64, in runs that record or replay
    /// preemptions: the store-exclusives the guest failed, each of which
    /// retried its loop and added retired branches, and those it executed.
    /// Host interrupts decide them, so they are shown in the full report but
    /// not in the INFO view `--verify` compares. Absent where not counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_exclusives: Option<StoreExclusiveCounts>,

    /// aarch64: why `store_exclusives` could not be counted in a run that
    /// records or replays preemptions. Full report only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_exclusives_unavailable: Option<String>,

    /// How the guest's process and thread IDs were numbered, for a run
    /// without a PID namespace of its own (`--no-namespace`): chosen from a
    /// base, or the host's. Absent for a run its PID namespace numbered. Shown
    /// in the full report as launch configuration, not in the INFO view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_ids: Option<crate::pid::GuestIds>,

    /// How many host-ID reports were accepted while IDs were to be chosen
    /// (`guest_ids`): nonzero means chosen IDs are not guaranteed. Shown in
    /// the full report beside `guest_ids`. This includes conservative reports
    /// before vfork calls, whose parents return only once the child has
    /// exec'd or exited. Failed and restarted attempts can therefore be
    /// counted too; this is not a count of distinct created tasks.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub host_id_tasks: u64,

    /// **Trace replay:** each departure from the record that
    /// `desync_descrip` summarizes, with where in the record it lies, so that
    /// a tool can place it without parsing text
    /// (<https://github.com/rrnewton/hermit/issues/4876>). Absent when there
    /// were none, and from summaries written before it existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub desync_notes: Vec<DesyncNote>,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// The store-exclusives a run's guest speculatively executed that failed,
/// and all it executed (aarch64 `STREX_FAIL_SPEC` and `STREX_SPEC`).
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct StoreExclusiveCounts {
    /// Store-exclusives that failed.
    pub failed: u64,
    /// Store-exclusives executed, where the core counts them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed: Option<u64>,
    /// Whether the counters were on a PMU counter for the whole run: not
    /// when the PMU multiplexed them, or the guest ran on CPUs whose PMU
    /// does not count these events. Then the counts cover only part of it.
    pub fully_counted: bool,
}

impl fmt::Display for StoreExclusiveCounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let StoreExclusiveCounts {
            failed,
            executed,
            fully_counted,
        } = self;
        let executed = executed.map(|executed| counted(executed, "store-exclusive"));
        match (*failed, executed) {
            (0, Some(executed)) => write!(f, "{executed} executed, none failed")?,
            (0, None) => write!(f, "No store-exclusive failed")?,
            (failed, executed) => {
                let failed = counted(failed, "store-exclusive failure");
                write!(f, "{failed}; replay may diverge")?;
                if let Some(executed) = executed {
                    write!(f, " ({executed} executed)")?;
                }
            }
        }
        if !fully_counted {
            write!(
                f,
                "; not counted for the whole run (multiplexed, or the task ran on CPUs whose \
                 PMU does not carry these events)"
            )?;
        }
        Ok(())
    }
}

/// `count` and `noun`, plural unless the count is 1.
fn counted(count: u64, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// What kind of departure a [`DesyncNote`] reports.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesyncKind {
    /// Recorded background starts the replay did not take.
    UntakenBackgroundStarts,
    /// A kind this build does not know, written by a later Hermit.
    #[serde(other)]
    Other,
}

/// Where a schedule replay was, in its own numbering and in the record's.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct ReplayPosition {
    /// The number of the event the replay traces next, as the `[tracereplay]
    /// ... at event #N` and `... before event #N` warnings number events
    /// (from 0).
    pub event: u64,
    /// The index, in the record's schedule (`global`, from 0), of the next
    /// recorded event the replay was to match. It equals `event` until the
    /// first desync; after one, an event observed in hard desync consumes no
    /// recorded event and a fast-forward skips recorded events, so the two
    /// can part.
    pub record_event: u64,
}

/// A recorded background start a [`DesyncNote`] concerns
/// (`background_starts` in the record).
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundStartRef {
    /// The thread whose call it began.
    pub thread: DetTid,
    /// The call's per-thread sequence number.
    pub call: u64,
    /// How many events of the record's schedule came before it: it lies
    /// before the record's event of this index.
    pub after: u64,
    /// Where the replay was when it passed the start without taking it (the
    /// next event came, or a fast-forward skipped past it): the observed event
    /// and the record's position before that event or the skip was consumed.
    /// For a start the replay passed at the next event, `record_event` is at
    /// least `after`; for one a fast-forward skipped, it is less, as the
    /// replay jumped over the start. `None` for a start still due when the
    /// replay ended (`after` at most the note's `reported_at.record_event`)
    /// and for one it never reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed_at: Option<ReplayPosition>,
}

/// One departure of a replay from its record, as `desync_descrip` lists it
/// (<https://github.com/rrnewton/hermit/issues/4876>).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DesyncNote {
    pub kind: DesyncKind,
    /// The thread it concerns, when it concerns one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<DetTid>,
    /// Where in the record the departure lies, as an index into the record's
    /// schedule (`global`, from 0): for `untaken_background_starts`, the
    /// first untaken start's `after`. `None` when the record gives none.
    pub record_event: Option<u64>,
    /// The recorded background starts it concerns
    /// (`untaken_background_starts`), in record order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_starts: Vec<BackgroundStartRef>,
    /// Where the replay was when it reported this: where it ended, for
    /// `untaken_background_starts`. `None` without a schedule replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<ReplayPosition>,
    /// The line `desync_descrip` carries for it.
    pub message: String,
}

/// The full human-readable report, including recorded launch configuration.
impl fmt::Display for RunSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_summary(f, true, self.reprio_descrip.as_deref())
    }
}

/// INFO view built with the producer's preemption description, before its host
/// destination is appended. Launch configuration is separate report metadata;
/// this view preserves the guest-behavior fields compared by `--verify`.
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
    /// Whether the histogram, when present, counts exactly the `syscalls`
    /// total. `None` when either is absent.
    pub fn syscall_histogram_is_consistent(&self) -> Option<bool> {
        let histogram = self.syscall_histogram.as_ref()?;
        Some(Some(histogram.total_calls()) == self.syscalls)
    }

    pub fn info<'a>(&'a self, reprio_description: Option<&'a str>) -> RunSummaryInfo<'a> {
        RunSummaryInfo {
            summary: self,
            reprio_description,
        }
    }

    fn fmt_summary(
        &self,
        f: &mut fmt::Formatter<'_>,
        include_report_metadata: bool,
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
            preemption,
            dispatch_stats: _,
            syscall_histogram: _,
            host_inputs: _,
            host_inputs_changed: _,
            exclusive_pairs,
            store_exclusives,
            store_exclusives_unavailable,
            guest_deadlock,
            guest_ids,
            host_id_tasks,
            virtual_timeout_ns,
            replay_departure: _,
            desync_notes: _,
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
        if *guest_deadlock {
            writeln!(
                f,
                "Deadlock under Hermit's model: no remaining thread can be woken by anything Hermit models."
            )?;
        }
        if let Some(bound) = virtual_timeout_ns {
            writeln!(
                f,
                "The run's virtual clock passed --virtual-timeout={bound}: no scheduler turn was granted after it."
            )?;
        }
        if include_report_metadata {
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

        if include_report_metadata && let Some(preemption) = preemption {
            writeln!(f, "{preemption}")?;
        }
        if include_report_metadata && let Some(guest_ids) = guest_ids {
            writeln!(f, "Guest process IDs: {guest_ids}")?;
            if *host_id_tasks > 0 {
                writeln!(
                    f,
                    "  {host_id_tasks} host-ID report(s) accepted; chosen process IDs are not guaranteed"
                )?;
            }
        }

        if include_report_metadata {
            if let Some(counts) = store_exclusives {
                writeln!(f, "{counts}")?;
            }
            if let Some(reason) = store_exclusives_unavailable {
                writeln!(f, "Store-exclusives were not counted: {reason}")?;
            }
        }
        if !exclusive_pairs.is_empty() {
            writeln!(
                f,
                "Store-exclusive (LL/SC) pairs in executable mappings of {} file(s):",
                exclusive_pairs.len()
            )?;
            for (path, pairs) in exclusive_pairs {
                writeln!(f, "  {path}: {pairs}")?;
            }
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
    use std::collections::BTreeMap;

    use super::PathEvidence;
    use super::PreemptionSummary;
    use super::RunSummary;
    use super::StoreExclusiveCounts;
    use super::TimesliceStats;

    #[test]
    fn replay_diagnostics_are_optional_json_and_never_change_compared_display() {
        use crate::replay_departure::Finding;
        use crate::replay_departure::Source;
        use crate::replay_departure::Summary;
        let plain = RunSummary::default();
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("replay_departure").is_none());
        let decoded: RunSummary = serde_json::from_value(json.clone()).unwrap();
        assert!(decoded.replay_departure.is_none());
        let mut marked = plain.clone();
        marked.replay_departure = Some(Summary {
            count: 2,
            first: Finding {
                source: Source::ThreadFiling,
                thread: crate::pid::DetTid::from_raw(3),
                previous: crate::time::LogicalTime::from_nanos(101),
                observed: crate::time::LogicalTime::from_nanos(100),
                slice: Some(7),
                recorded_event_count: Some(19),
            },
        });
        assert_eq!(marked.to_string(), plain.to_string());
        assert_eq!(marked.info(None).to_string(), plain.info(None).to_string());
        let mut marked_json = serde_json::to_value(&marked).unwrap();
        let roundtrip: RunSummary = serde_json::from_value(marked_json.clone()).unwrap();
        assert_eq!(roundtrip.replay_departure, marked.replay_departure);
        assert_eq!(
            marked_json
                .as_object_mut()
                .unwrap()
                .remove("replay_departure"),
            Some(serde_json::to_value(marked.replay_departure.unwrap()).unwrap())
        );
        assert_eq!(
            marked_json, json,
            "every existing JSON field stays identical"
        );
    }

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

    #[test]
    fn older_summary_json_keeps_preemption_configuration_unknown() {
        let value = serde_json::to_value(RunSummary::default()).unwrap();
        assert!(value.get("preemption").is_none());
        let parsed: RunSummary = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.preemption, None);

        let mut value = serde_json::to_value(&parsed).unwrap();
        value["preemption"] = serde_json::json!({ "max_timeslice_ns": 200_000_000 });
        let parsed: RunSummary = serde_json::from_value(value).unwrap();
        assert_eq!(
            parsed.preemption,
            Some(PreemptionSummary {
                max_timeslice_ns: Some(200_000_000),
                timer_enabled: None,
                stopless_slice_ends: None,
            })
        );

        let mut value = serde_json::to_value(&parsed).unwrap();
        value["preemption"] = serde_json::Value::Null;
        let parsed: RunSummary = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.preemption, None);
    }

    #[test]
    fn summary_json_distinguishes_disabled_and_positive_preemption() {
        for (max_timeslice_ns, timer_enabled, expected) in [
            (
                None,
                Some(false),
                serde_json::json!({ "max_timeslice_ns": null, "timer_enabled": false }),
            ),
            (
                Some(200_000_000),
                Some(false),
                serde_json::json!({ "max_timeslice_ns": 200_000_000, "timer_enabled": false }),
            ),
            (
                Some(12_340),
                Some(true),
                serde_json::json!({ "max_timeslice_ns": 12_340, "timer_enabled": true }),
            ),
            (
                Some(200_000_000),
                None,
                serde_json::json!({ "max_timeslice_ns": 200_000_000, "timer_enabled": null }),
            ),
        ] {
            let summary = RunSummary {
                preemption: Some(PreemptionSummary {
                    max_timeslice_ns,
                    timer_enabled,
                    stopless_slice_ends: None,
                }),
                ..Default::default()
            };
            let value = serde_json::to_value(&summary).unwrap();
            assert_eq!(value["preemption"], expected);
            let parsed: RunSummary = serde_json::from_value(value).unwrap();
            assert_eq!(parsed.preemption, summary.preemption);
        }
    }

    /// `desync_notes` reaches `--summary-json` with each note's kind,
    /// recorded position, background starts and replay position, and reads
    /// back; a summary without notes, or written before the field existed,
    /// omits it; a kind from a later Hermit still parses
    /// (https://github.com/rrnewton/hermit/issues/4876).
    #[test]
    fn desync_notes_round_trip_through_summary_json() {
        use super::BackgroundStartRef;
        use super::DesyncKind;
        use super::DesyncNote;
        use super::ReplayPosition;
        use crate::pid::DetTid;
        let summary = RunSummary {
            desync_notes: vec![DesyncNote {
                kind: DesyncKind::UntakenBackgroundStarts,
                thread: None,
                record_event: Some(51206),
                background_starts: vec![
                    BackgroundStartRef {
                        thread: DetTid::from_raw(5121),
                        call: 35,
                        after: 51206,
                        passed_at: Some(ReplayPosition {
                            event: 51207,
                            record_event: 51206,
                        }),
                    },
                    BackgroundStartRef {
                        thread: DetTid::from_raw(5123),
                        call: 24,
                        after: 51217,
                        passed_at: None,
                    },
                ],
                reported_at: Some(ReplayPosition {
                    event: 51271,
                    record_event: 51217,
                }),
                message: "DESYNC 2 recorded background starts were not taken: ...".into(),
            }],
            ..Default::default()
        };
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            value["desync_notes"],
            serde_json::json!([{
                "kind": "untaken_background_starts",
                "record_event": 51206,
                "background_starts": [
                    {
                        "thread": 5121,
                        "call": 35,
                        "after": 51206,
                        "passed_at": { "event": 51207, "record_event": 51206 }
                    },
                    { "thread": 5123, "call": 24, "after": 51217 }
                ],
                "reported_at": { "event": 51271, "record_event": 51217 },
                "message": "DESYNC 2 recorded background starts were not taken: ..."
            }])
        );
        let parsed: RunSummary = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(parsed.desync_notes, summary.desync_notes);

        let mut later = value;
        later["desync_notes"][0]["kind"] = serde_json::json!("callback_checkpoint");
        let parsed: RunSummary = serde_json::from_value(later).unwrap();
        assert_eq!(parsed.desync_notes[0].kind, DesyncKind::Other);

        let none = serde_json::to_value(RunSummary::default()).unwrap();
        assert!(none.get("desync_notes").is_none(), "{none}");
        let old: RunSummary = serde_json::from_value(none).unwrap();
        assert!(old.desync_notes.is_empty());
    }

    #[test]
    fn full_summary_shows_effective_preemption_without_changing_guest_info() {
        let baseline = RunSummary {
            syscalls: Some(7),
            ..Default::default()
        };
        let baseline_info = baseline.info(None).to_string();
        assert!(baseline_info.contains("Guest threads completed 7 syscalls."));
        assert!(
            !baseline
                .to_string()
                .contains("Configured maximum timeslice:")
        );
        for (max_timeslice_ns, timer_enabled, expected) in [
            (
                None,
                Some(false),
                "Configured maximum timeslice: disabled; timer: none",
            ),
            (
                Some(12_340),
                Some(true),
                "Configured maximum timeslice: 12340 virtual ns; timer: enabled",
            ),
            (
                Some(200_000_000),
                Some(false),
                "Configured maximum timeslice: 200000000 virtual ns; timer: none",
            ),
            (
                Some(200_000_000),
                None,
                "Configured maximum timeslice: 200000000 virtual ns; timer: unknown",
            ),
        ] {
            let preemption = PreemptionSummary {
                max_timeslice_ns,
                timer_enabled,
                stopless_slice_ends: None,
            };
            assert_eq!(preemption.to_string(), expected);
            let summary = RunSummary {
                preemption: Some(preemption),
                ..baseline.clone()
            };
            assert!(summary.to_string().contains(expected));
            assert_eq!(summary.info(None).to_string(), baseline_info);
        }
    }

    #[test]
    fn a_count_of_slice_ends_without_a_stop_is_shown_when_known() {
        let preemption = PreemptionSummary {
            max_timeslice_ns: Some(1000),
            timer_enabled: Some(true),
            stopless_slice_ends: Some(42),
        };
        assert_eq!(
            preemption.to_string(),
            "Configured maximum timeslice: 1000 virtual ns; timer: enabled; \
             slice ends taken without a stop (no other thread could run): 42"
        );
        let value = serde_json::to_value(preemption).unwrap();
        assert_eq!(value["stopless_slice_ends"], 42);
        let unknown = PreemptionSummary {
            stopless_slice_ends: None,
            ..preemption
        };
        assert!(
            serde_json::to_value(unknown)
                .unwrap()
                .get("stopless_slice_ends")
                .is_none()
        );
    }

    #[test]
    fn store_exclusive_pairs_are_in_both_views_and_counts_only_in_the_full_report() {
        let baseline = RunSummary {
            syscalls: Some(7),
            ..Default::default()
        };
        let value = serde_json::to_value(&baseline).unwrap();
        for key in [
            "exclusive_pairs",
            "store_exclusives",
            "store_exclusives_unavailable",
        ] {
            assert!(value.get(key).is_none(), "{key}");
        }
        assert!(!baseline.to_string().contains("tore-exclusive"));

        let pairs = RunSummary {
            exclusive_pairs: BTreeMap::from([
                ("/lib64/libc.so.6".to_owned(), 38),
                ("/bin/guest".to_owned(), 2),
            ]),
            ..baseline.clone()
        };
        let listing = "Store-exclusive (LL/SC) pairs in executable mappings of 2 file(s):\n  \
                       /bin/guest: 2\n  /lib64/libc.so.6: 38\n";
        assert!(pairs.info(None).to_string().contains(listing));
        assert!(pairs.to_string().contains(listing));

        for (counts, line, json) in [
            (
                StoreExclusiveCounts {
                    failed: 20,
                    executed: Some(10_000_020),
                    fully_counted: true,
                },
                "20 store-exclusive failures; replay may diverge (10000020 store-exclusives \
                 executed)\n",
                serde_json::json!({"failed": 20, "executed": 10_000_020, "fully_counted": true}),
            ),
            (
                StoreExclusiveCounts {
                    failed: 0,
                    executed: Some(2),
                    fully_counted: true,
                },
                "2 store-exclusives executed, none failed\n",
                serde_json::json!({"failed": 0, "executed": 2, "fully_counted": true}),
            ),
            (
                StoreExclusiveCounts {
                    failed: 0,
                    executed: Some(1),
                    fully_counted: true,
                },
                "1 store-exclusive executed, none failed\n",
                serde_json::json!({"failed": 0, "executed": 1, "fully_counted": true}),
            ),
            (
                StoreExclusiveCounts {
                    failed: 3,
                    executed: None,
                    fully_counted: true,
                },
                "3 store-exclusive failures; replay may diverge\n",
                serde_json::json!({"failed": 3, "fully_counted": true}),
            ),
            (
                StoreExclusiveCounts {
                    failed: 0,
                    executed: None,
                    fully_counted: true,
                },
                "No store-exclusive failed\n",
                serde_json::json!({"failed": 0, "fully_counted": true}),
            ),
            (
                StoreExclusiveCounts {
                    failed: 1,
                    executed: Some(9),
                    fully_counted: false,
                },
                "1 store-exclusive failure; replay may diverge (9 store-exclusives executed); \
                 not counted for the whole run (multiplexed, or the task ran on CPUs whose PMU \
                 does not carry these events)\n",
                serde_json::json!({"failed": 1, "executed": 9, "fully_counted": false}),
            ),
        ] {
            let counted = RunSummary {
                store_exclusives: Some(counts),
                ..pairs.clone()
            };
            assert!(counted.to_string().contains(line), "{counted}");
            assert_eq!(counted.info(None).to_string(), pairs.info(None).to_string());
            let value = serde_json::to_value(&counted).unwrap();
            assert_eq!(value["store_exclusives"], json);
            let parsed: RunSummary = serde_json::from_value(value).unwrap();
            assert_eq!(parsed.store_exclusives, Some(counts));
            assert_eq!(parsed.exclusive_pairs, pairs.exclusive_pairs);
        }

        let unavailable = RunSummary {
            store_exclusives_unavailable: Some("no PMU".to_owned()),
            ..pairs.clone()
        };
        assert!(
            unavailable
                .to_string()
                .contains("Store-exclusives were not counted: no PMU\n")
        );
        assert_eq!(
            unavailable.info(None).to_string(),
            pairs.info(None).to_string()
        );
    }
}
