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
use crate::host_input::StdioChange;
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

/// The post-crash timeslice switch of a run (`Config::post_crash_timeslice`).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct PostCrashSummary {
    /// The post-crash bound in effect, in virtual nanoseconds.
    pub bound: u64,
    /// Where each switch was taken, in order; empty when the run never
    /// crashed.
    #[serde(default)]
    pub switches: Vec<PostCrashSwitchSummary>,
    /// Under a preemption replay, how many switches its record holds that the
    /// replay never took (the replay left its record before reaching them).
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub recorded_not_taken: usize,
}

fn is_zero_usize(count: &usize) -> bool {
    *count == 0
}

/// Where a run took the post-crash switch.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct PostCrashSwitchSummary {
    /// The thread the crash signal was delivered to.
    pub dtid: i32,
    /// The crash signal's number.
    pub signal: i32,
    /// The thread whose `execve` took the switch, or `None` when the
    /// delivery took the default action.
    pub exec_by: Option<i32>,
    /// The last committed scheduler turn when it was taken.
    pub turn: u64,
}

impl fmt::Display for PostCrashSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Post-crash timeslice: {} virtual ns, ", self.bound)?;
        if self.switches.is_empty() {
            write!(f, "not taken")?;
        }
        for (index, switch) in self.switches.iter().enumerate() {
            if index > 0 {
                write!(f, "; ")?;
            }
            write!(
                f,
                "switched at turn {} on signal {} to dtid {}",
                switch.turn, switch.signal, switch.dtid
            )?;
            match switch.exec_by {
                Some(dtid) => write!(f, " (taken at dtid {dtid}'s execve)")?,
                None => write!(f, " (default action)")?,
            }
        }
        if self.recorded_not_taken > 0 {
            write!(
                f,
                "; {} recorded switch(es) not taken by this replay",
                self.recorded_not_taken
            )?;
        }
        Ok(())
    }
}

/// The Android system property snapshot a run's guest read
/// (`Config::property_snapshot`).
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct PropertySnapshotSummary {
    /// How many files of the area were copied.
    pub copied_files: u64,
    /// How many files stopped being readable mid-capture and have no copy.
    pub uncopied_files: u64,
    /// Whether the copies are one consistent cut (no property set during
    /// the capture) rather than coherent per file only.
    pub consistent_cut: bool,
}

impl fmt::Display for PropertySnapshotSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.consistent_cut {
            "one consistent cut"
        } else {
            "coherent per file only (properties changed during the capture)"
        };
        write!(
            f,
            "System property snapshot: {} files copied, {kind}",
            self.copied_files
        )?;
        if self.uncopied_files > 0 {
            write!(
                f,
                "; {} files without a copy (refused if opened)",
                self.uncopied_files
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

    /// The post-crash timeslice switch, when the run had it on
    /// (`Config::post_crash_timeslice`). Shown in the full report, but not
    /// the INFO guest-behavior view used by `--verify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_crash: Option<PostCrashSummary>,

    /// The system property snapshot the guest read, when the run took one.
    /// Launch metadata, so not part of the INFO view `--verify` compares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property_snapshot: Option<PropertySnapshotSummary>,

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

    /// aarch64: the reads of the generic timer's virtual counter from EL0
    /// (`MRS Xt, CNTVCT_EL0` or `CNTVCTSS_EL0`) in the guest's executable
    /// file mappings, by guest path, for the files that have any. Such a
    /// read makes no stop Hermit sees, so Detcore cannot virtualize it and a
    /// guest whose control flow depends on one is not deterministic. A
    /// function of the files' bytes, so it is in the INFO view. Empty
    /// elsewhere.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counter_reads: BTreeMap<String, u64>,

    /// aarch64, `--rewrite-exclusives` in a run without a mount namespace:
    /// what rewriting the exclusive loops in the guest's memory did to each
    /// guest path it touched. A function of the files' bytes and the guest's
    /// mappings, so it is in the INFO view. Empty elsewhere.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exclusive_rewrites: BTreeMap<String, ExclusiveRewrite>,

    /// aarch64, the in-memory rewrite: the most its plan cache held at once
    /// during the run, in bytes: each plan with the file bytes it was made
    /// from, and the cache's own containers. The cache starts empty at each run, but
    /// it belongs to the Hermit process, so this is shown in the full report
    /// and not in the INFO view `--verify` compares. Zero elsewhere.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub exclusive_rewrite_cache_peak: u64,

    /// **Nondeterministic:** aarch64, in runs that record or replay
    /// preemptions and in each run of `--verify`: the store-exclusives the
    /// guest failed, each of which retried its loop and added retired
    /// branches, and those it executed.
    /// Host interrupts decide them, so they are shown in the full report but
    /// not in the INFO view `--verify` compares. Absent where not counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_exclusives: Option<StoreExclusiveCounts>,

    /// aarch64, `--rewrite-exclusives` with its overlay: the load/store
    /// exclusive pairs of each mapped file the overlay was not given (it
    /// rewrites the executable and each `--rewrite-exclusives-also` file),
    /// by guest path, for the files that have any. A function of the files'
    /// bytes and the run's options, so it is in the INFO view. Empty
    /// elsewhere.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exclusive_pairs_not_rewritten: BTreeMap<String, u64>,

    /// aarch64: why `store_exclusives` could not be counted in a run that
    /// counts them. Full report only.
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

    /// How many binder calls ran as external input: a call that waits for
    /// another process runs as a blocking external call, so when it finishes
    /// is this host's. Absent when there were none. Shown in the full report,
    /// not the INFO guest-behavior view used by `--verify`: which calls go to
    /// the background is itself decided by host timing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binder_external_calls: Option<u64>,
    /// The guest had not exited when `--timeout`, the run's wall-clock
    /// deadline, whose bound in seconds this is, stopped the run: the summary
    /// is partial and describes the run up to then, including the exit
    /// reports of the threads the deadline killed. Absent for every other run
    /// and in older summaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_timeout_secs: Option<u64>,

    /// The guest's standard descriptors whose kind (file, pipe, terminal, ...)
    /// a schedule replay found different from the recording it replays, in
    /// descriptor order (see [`crate::host_input::StdioChange`]); this run's
    /// own kinds are in `host_inputs`. Empty when they match, when either run
    /// did not look, or the run replayed nothing. Not part of the
    /// human-readable summary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stdio_changed: Vec<StdioChange>,

    /// Skid-overshoot events of the run's precise PMU timers: preemption
    /// points that took effect past the branch count they were programmed
    /// for, because the overflow interrupt was handled late. One event is
    /// one thread's timer and its target, however many sites reported it
    /// (`detcore::skid_overshoots`). Absent where the backend has no such
    /// timers. Shown in the full report when nonzero, but not in the INFO
    /// view used by `--verify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skid_overshoots: Option<u64>,

    /// The detection sites' reports of those events, Reverie's and Detcore's
    /// prehook's, which usually both report one event
    /// (`reverie::take_skid_overshoot_count`). Present exactly when
    /// `skid_overshoots` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skid_overshoot_reports: Option<u64>,

    /// `false` when this execution was not reproducible from its seed and
    /// configuration: a skid overshoot moved a preemption point in this run by
    /// host interrupt latency. It describes this execution, never the seed: the
    /// same seed can run clean once and overshoot the next time, so no seed is
    /// classified by it. Absent otherwise, which claims nothing either way: an
    /// absent field with `skid_overshoots: 0` means that no overshoot was
    /// observed in this run, not that the seed reproduces it; see
    /// [`RunSummary::record_skid_overshoots`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_reproducible: Option<bool>,
    /// `--watch`: which threads armed their hardware watchpoints, and the hits.
    /// Shown in the full report, not the INFO view, so a watch leaves what
    /// `--verify` compares as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchRunSummary>,

    /// aarch64: the files the run's `--rewrite-exclusives` left unchanged,
    /// wholly or in part, because no mapping symbol it accepts marks their
    /// code: none, or one that is neither `$x` nor `$d`. Shown in the INFO view and the
    /// full report. Absent when there were none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclusive_rewrite_unmarked: Vec<String>,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// What rewriting one file's exclusive loops in a guest's memory did. Every
/// exclusive pair the scan found in the file's executable mappings is in
/// exactly one count.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct ExclusiveRewrite {
    /// Loops the guest's memory holds as single LSE atomics.
    pub rewritten: u64,
    /// Loops left as the file has them: refused by the rewriter, in a shared
    /// or writable mapping, crossing a mapping's end, or not the file's bytes
    /// in memory.
    pub left: u64,
    /// Loops the rewrite could not write; a mapping that could not be read
    /// counts one. These still retry.
    pub failed: u64,
    /// Why loops were not written, where any were not (`failed`): for
    /// example that the mapped file cannot be proven to be the one planned
    /// without `/proc/<pid>/map_files` on btrfs or overlayfs. Absent when
    /// every loop asked for was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_written: Option<String>,
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
    /// For how much of the run the less covered counter was on a PMU
    /// counter, in thousandths of the time it was enabled: 1000 when
    /// `fully_counted`. A count of 0 over most of the run observed none,
    /// over a sliver of it little; only a whole run rules a rare one out.
    /// Absent from summaries written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counted_per_mille: Option<u32>,
}

impl StoreExclusiveCounts {
    /// What part of the run the counts cover, for a run not `fully_counted`:
    /// the share when it is known.
    pub fn coverage(&self) -> String {
        let why = "the PMU multiplexed the counters, or the task ran on CPUs whose PMU does \
                   not carry these events";
        match self.counted_per_mille {
            Some(per_mille) => format!(
                "counted for {} of the run ({why})",
                counted_share(per_mille)
            ),
            None => format!("not counted for the whole run ({why})"),
        }
    }
}

/// A coverage share in thousandths as a percentage with one decimal: "less
/// than 0.1%" for 0, which a counter that ran at all rounds down to.
pub fn counted_share(per_mille: u32) -> String {
    if per_mille == 0 {
        "less than 0.1%".to_owned()
    } else {
        format!("{}.{}%", per_mille / 10, per_mille % 10)
    }
}

impl fmt::Display for StoreExclusiveCounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let StoreExclusiveCounts {
            failed,
            executed,
            fully_counted,
            counted_per_mille: _,
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
            write!(f, "; {}", self.coverage())?;
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
    /// The earliest replayed schedule event that differed from the record.
    /// `schedevent_desynced` counts every such event; only a thread mismatch
    /// also prints a `[tracereplay]` warning.
    ScheduleEvent,
    /// A thread reached a recorded callback checkpoint (the end of a recorded
    /// slice at a callback) with other coordinates than the record's.
    CallbackCheckpoint,
    /// The run ended with recorded callback checkpoints of a thread never
    /// reached.
    IncompleteCallbackHistory,
    /// Recorded background starts the replay did not take.
    UntakenBackgroundStarts,
    /// Recorded jumps of global time the replay could not make as recorded
    /// (made otherwise than the recording, or not made), never reached, or
    /// still unconfirmed when it ended.
    GlobalTimeJumps,
    /// A kind this build does not know, written by a later Hermit.
    #[serde(other)]
    Other,
}

/// A recorded callback checkpoint a [`DesyncNote`] concerns.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointRef {
    /// Its number in the thread's list (`callback_checkpoints` in the
    /// record).
    pub number: usize,
    /// The index, in the record's schedule (`global`, from 0), of the
    /// thread's event that ends the checkpoint's slice. `None` for a
    /// checkpoint before the thread's first event.
    pub record_event: Option<u64>,
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
    /// The thread it concerns, when it concerns one: the thread that ran the
    /// differing event, or whose checkpoints these are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<DetTid>,
    /// Where in the record the departure lies, as an index into the record's
    /// schedule (`global`, from 0): the recorded event the first differing
    /// event stood for (`schedule_event`); the event ending the checkpoint's
    /// slice (`callback_checkpoint`); the first unconsumed checkpoint's
    /// (`incomplete_callback_history`); the first untaken start's `after`
    /// (`untaken_background_starts`); the earliest of those recorded jumps'
    /// places, before the record's event of that index (`global_time_jumps`).
    /// `None` when the record gives none, as a boundary-protocol record does
    /// not for a jump, whose place there counts its boundary records too.
    pub record_event: Option<u64>,
    /// The recorded callback checkpoints it concerns, each with the index of
    /// the event ending its slice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<CheckpointRef>,
    /// The recorded background starts it concerns
    /// (`untaken_background_starts`), in record order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_starts: Vec<BackgroundStartRef>,
    /// Where the replay was when it reported this: at the differing event
    /// itself (`schedule_event`); when the thread reported the checkpoint,
    /// which can be later than `record_event`, since a thread reports it on
    /// its next check-in (`callback_checkpoint`); where the replay ended
    /// (`incomplete_callback_history`, `untaken_background_starts`,
    /// `global_time_jumps`). `None` without a schedule replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<ReplayPosition>,
    /// The line `desync_descrip` carries for it.
    pub message: String,
}

/// The `--watch` part of a [`RunSummary`].
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct WatchRunSummary {
    /// Threads that armed their watches in at least one image interval.
    pub armed: u64,
    /// Intervals that never did, by thread and why. A thread can appear
    /// here for a post-exec interval and still count in `armed` for an
    /// earlier one.
    pub unarmed: Vec<(DetTid, crate::access_trace::WatchUnarmed)>,
    /// Hits on every thread.
    pub hits: u64,
}

impl fmt::Display for WatchRunSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Watch: {} thread(s) armed their watches, {} hit(s)",
            self.armed, self.hits
        )?;
        if !self.unarmed.is_empty() {
            let unarmed: Vec<String> = self
                .unarmed
                .iter()
                .map(|(tid, why)| format!("thread {tid} ({why:?})"))
                .collect();
            write!(f, "; uncovered intervals: {}", unarmed.join(", "))?;
        }
        writeln!(f)
    }
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
    /// aarch64: the exclusive loops of each guest path that could still
    /// retry in this run, for the paths that have any: every pair the scan
    /// found ([`Self::exclusive_pairs`]), less what an in-memory rewrite made
    /// single LSE atomics ([`Self::exclusive_rewrites`]). Under an overlay
    /// rewrite the scan reads the rewritten copy, so its pairs are already
    /// only the loops the rewrite left.
    pub fn exclusive_pairs_left(&self) -> BTreeMap<String, u64> {
        // Every path either map names: an in-memory rewrite also records
        // paths whose scan found no pair it could attribute (a loop crossing
        // a mapping's end, a mapping that could not be read).
        self.exclusive_pairs
            .keys()
            .chain(self.exclusive_rewrites.keys())
            .map(|path| {
                let left = match self.exclusive_rewrites.get(path) {
                    Some(rewrite) => rewrite.left + rewrite.failed,
                    None => self.exclusive_pairs[path],
                };
                (path.clone(), left)
            })
            .filter(|(_, left)| *left != 0)
            .collect()
    }

    /// Whether the histogram, when present, counts exactly the `syscalls`
    /// total. `None` when either is absent.
    pub fn syscall_histogram_is_consistent(&self) -> Option<bool> {
        let histogram = self.syscall_histogram.as_ref()?;
        Some(Some(histogram.total_calls()) == self.syscalls)
    }

    /// Records the skid-overshoot `events` of a run whose backend has precise
    /// PMU timers, and the `reports` of them by its detection sites; there
    /// are events exactly when there are reports. Any report means a
    /// preemption point of this execution moved by host interrupt latency, so
    /// its seed does not reproduce this execution; another run of the same
    /// seed may not overshoot. None proves no more than that no overshoot was
    /// observed in this run: it is no reproducibility guarantee, and a relaxed
    /// configuration may still not be reproducible, so `seed_reproducible` is
    /// then left absent rather than set to `true`.
    pub fn record_skid_overshoots(&mut self, events: u64, reports: u64) {
        debug_assert!(events <= reports && (events > 0) == (reports > 0));
        self.skid_overshoots = Some(events);
        self.skid_overshoot_reports = Some(reports);
        self.seed_reproducible = (reports > 0).then_some(false);
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
            post_crash,
            property_snapshot,
            dispatch_stats: _,
            syscall_histogram: _,
            host_inputs: _,
            host_inputs_changed: _,
            exclusive_pairs,
            counter_reads,
            exclusive_rewrites,
            exclusive_rewrite_cache_peak,
            store_exclusives,
            exclusive_pairs_not_rewritten,
            store_exclusives_unavailable,
            guest_deadlock,
            guest_ids,
            host_id_tasks,
            virtual_timeout_ns,
            replay_departure: _,
            desync_notes: _,
            binder_external_calls,
            run_timeout_secs,
            skid_overshoots,
            skid_overshoot_reports,
            seed_reproducible: _,
            watch,
            stdio_changed: _,
            exclusive_rewrite_unmarked,
        } = self;
        if let Some(bound) = run_timeout_secs {
            writeln!(
                f,
                "PARTIAL: --timeout={bound} (seconds of wall-clock time) stopped the run before the guest exited; these statistics describe the run up to then."
            )?;
        }
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
        if include_report_metadata && let Some(calls) = binder_external_calls {
            writeln!(
                f,
                "{calls} binder call(s) ran as external input: when they finished was this host's"
            )?;
        }
        if include_report_metadata
            && let Some(events) = skid_overshoots.filter(|&events| events > 0)
        {
            let reports = match skid_overshoot_reports {
                Some(reports) if *reports != events => format!(" ({reports} reports)"),
                _ => String::new(),
            };
            writeln!(
                f,
                "Skid overshoots: {events} event(s){reports}; a precise timer took effect past \
                 its programmed branch count, so this run is not reproducible from its seed \
                 (the overshoot moved a preemption point in this execution; another run of the \
                 same seed may not overshoot)"
            )?;
        }
        if include_report_metadata && let Some(post_crash) = post_crash {
            writeln!(f, "{post_crash}")?;
        }
        if include_report_metadata && let Some(watch) = watch {
            write!(f, "{watch}")?;
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
        if !exclusive_pairs_not_rewritten.is_empty() {
            writeln!(
                f,
                "Not tried by --rewrite-exclusives, whose overlay rewrites only the executable and \
                 each --rewrite-exclusives-also file (name a file with --rewrite-exclusives-also \
                 to have it try its loops), in {} file(s):",
                exclusive_pairs_not_rewritten.len()
            )?;
            for (path, pairs) in exclusive_pairs_not_rewritten {
                writeln!(f, "  {path}: {pairs}")?;
            }
        }
        if !counter_reads.is_empty() {
            writeln!(
                f,
                "EL0 generic-timer counter reads, which Detcore cannot virtualize, in executable \
                 mappings of {} file(s):",
                counter_reads.len()
            )?;
            for (path, reads) in counter_reads {
                writeln!(f, "  {path}: {reads}")?;
            }
        }
        if !exclusive_rewrite_unmarked.is_empty() {
            writeln!(
                f,
                "--rewrite-exclusives left {} file(s) unchanged where no mapping symbol it \
                 accepts marks their code:",
                exclusive_rewrite_unmarked.len()
            )?;
            for path in exclusive_rewrite_unmarked {
                writeln!(f, "  {path}")?;
            }
        }
        if !exclusive_rewrites.is_empty() {
            write!(
                f,
                "Exclusive loops rewritten in the guest's memory, in {} file(s)",
                exclusive_rewrites.len()
            )?;
            if include_report_metadata && *exclusive_rewrite_cache_peak != 0 {
                write!(
                    f,
                    " (plan cache peak: {exclusive_rewrite_cache_peak} bytes)"
                )?;
            }
            writeln!(f, ":")?;
            for (path, rewrite) in exclusive_rewrites {
                let ExclusiveRewrite {
                    rewritten,
                    left,
                    failed,
                    not_written,
                } = rewrite;
                write!(
                    f,
                    "  {path}: {rewritten} rewritten, {left} left, {failed} not written"
                )?;
                match not_written {
                    Some(why) => writeln!(f, " ({why})")?,
                    None => writeln!(f)?,
                }
            }
        }
        if include_report_metadata && let Some(snapshot) = property_snapshot {
            writeln!(f, "{snapshot}")?;
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

    use super::ExclusiveRewrite;
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
    fn the_loops_left_are_the_pairs_less_what_the_in_memory_rewrite_made_atomic() {
        let summary = RunSummary {
            // Scanned without an in-memory rewrite (or an overlay copy whose
            // pairs are what the rewrite left): every pair is left.
            exclusive_pairs: BTreeMap::from([
                ("/bin/guest".to_owned(), 4),
                ("/lib/libc.so.6".to_owned(), 19),
                ("/lib/ld.so".to_owned(), 3),
            ]),
            exclusive_rewrites: BTreeMap::from([
                (
                    "/lib/libc.so.6".to_owned(),
                    ExclusiveRewrite {
                        rewritten: 16,
                        left: 2,
                        failed: 1,
                        not_written: None,
                    },
                ),
                (
                    "/lib/ld.so".to_owned(),
                    ExclusiveRewrite {
                        rewritten: 3,
                        left: 0,
                        failed: 0,
                        not_written: None,
                    },
                ),
                // A mapping that could not be read: no pair scanned, one
                // loop that could not be written.
                (
                    "/lib/unreadable.so".to_owned(),
                    ExclusiveRewrite {
                        rewritten: 0,
                        left: 0,
                        failed: 1,
                        not_written: None,
                    },
                ),
            ]),
            ..Default::default()
        };
        assert_eq!(
            summary.exclusive_pairs_left(),
            BTreeMap::from([
                ("/bin/guest".to_owned(), 4),
                ("/lib/libc.so.6".to_owned(), 3),
                ("/lib/unreadable.so".to_owned(), 1),
            ])
        );
        assert!(RunSummary::default().exclusive_pairs_left().is_empty());
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

    /// A run `--timeout` stopped says so first, in the report and in the INFO
    /// view, and its JSON names the bound; any other run, and an older
    /// summary, carries no bound.
    #[test]
    fn a_summary_of_a_run_the_timeout_stopped_is_marked_partial() {
        let finished = RunSummary::default();
        assert!(!finished.to_string().contains("PARTIAL"));
        let value = serde_json::to_value(&finished).unwrap();
        assert!(value.get("run_timeout_secs").is_none());
        let parsed: RunSummary = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.run_timeout_secs, None);

        let stopped = RunSummary {
            run_timeout_secs: Some(2),
            ..Default::default()
        };
        let partial = "PARTIAL: --timeout=2 (seconds of wall-clock time) stopped the run";
        for text in [stopped.to_string(), stopped.info(None).to_string()] {
            assert!(text.starts_with(partial), "{text}");
        }
        let value = serde_json::to_value(&stopped).unwrap();
        assert_eq!(value["run_timeout_secs"], 2);
        let parsed: RunSummary = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.run_timeout_secs, Some(2));
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

    // A summary records skid-overshoot events and their reports only where
    // a backend counted them. Any marks the run as not reproducible from its
    // seed, in the JSON and in the full report, which names the reports when
    // they outnumber the events; none leaves `seed_reproducible` absent,
    // which claims nothing. The INFO view `--verify` compares never shows
    // them.
    #[test]
    fn skid_overshoots_mark_a_run_not_reproducible_from_its_seed() {
        let baseline = RunSummary::default();
        let value = serde_json::to_value(&baseline).unwrap();
        assert!(value.get("skid_overshoots").is_none());
        assert!(value.get("skid_overshoot_reports").is_none());
        assert!(value.get("seed_reproducible").is_none());

        let mut clean = baseline.clone();
        clean.record_skid_overshoots(0, 0);
        let value = serde_json::to_value(&clean).unwrap();
        assert_eq!(value["skid_overshoots"], 0);
        assert_eq!(value["skid_overshoot_reports"], 0);
        assert!(value.get("seed_reproducible").is_none());
        assert_eq!(clean.to_string(), baseline.to_string());

        // Three events, each reported by Reverie and by Detcore's prehook.
        let mut overshot = baseline.clone();
        overshot.record_skid_overshoots(3, 6);
        let value = serde_json::to_value(&overshot).unwrap();
        assert_eq!(value["skid_overshoots"], 3);
        assert_eq!(value["skid_overshoot_reports"], 6);
        assert_eq!(value["seed_reproducible"], false);
        let parsed: RunSummary = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.skid_overshoots, Some(3));
        assert_eq!(parsed.skid_overshoot_reports, Some(6));
        assert_eq!(parsed.seed_reproducible, Some(false));
        assert!(
            overshot.to_string().contains(
                "Skid overshoots: 3 event(s) (6 reports); a precise timer took effect past its \
                 programmed branch count, so this run is not reproducible from its seed (the \
                 overshoot moved a preemption point in this execution; another run of the same \
                 seed may not overshoot)\n"
            ),
            "{overshot}"
        );
        assert_eq!(
            overshot.info(None).to_string(),
            baseline.info(None).to_string()
        );

        // One site alone reported each event.
        let mut single = baseline.clone();
        single.record_skid_overshoots(2, 2);
        let value = serde_json::to_value(&single).unwrap();
        assert_eq!(value["skid_overshoots"], 2);
        assert_eq!(value["skid_overshoot_reports"], 2);
        assert_eq!(value["seed_reproducible"], false);
        assert!(
            single.to_string().contains(
                "Skid overshoots: 2 event(s); a precise timer took effect past its programmed \
                 branch count"
            ),
            "{single}"
        );
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

    /// `desync_notes` reaches `--summary-json` with each note's kind, thread,
    /// recorded position, checkpoints and replay position, and reads back; a
    /// summary without notes, or written before the field existed, omits it;
    /// a kind from a later Hermit still parses
    /// (https://github.com/rrnewton/hermit/issues/4876).
    #[test]
    fn desync_notes_round_trip_through_summary_json() {
        use super::CheckpointRef;
        use super::DesyncKind;
        use super::DesyncNote;
        use super::ReplayPosition;
        use crate::pid::DetTid;
        let summary = RunSummary {
            desync_notes: vec![
                DesyncNote {
                    kind: DesyncKind::CallbackCheckpoint,
                    thread: Some(DetTid::from_raw(5125)),
                    record_event: Some(16559),
                    checkpoints: vec![CheckpointRef {
                        number: 23,
                        record_event: Some(16559),
                    }],
                    background_starts: Vec::new(),
                    reported_at: Some(ReplayPosition {
                        event: 16621,
                        record_event: 16617,
                    }),
                    message: "DESYNC callback checkpoint 23 for 5125 at record event #16559".into(),
                },
                DesyncNote {
                    kind: DesyncKind::IncompleteCallbackHistory,
                    thread: Some(DetTid::from_raw(5126)),
                    record_event: None,
                    checkpoints: vec![CheckpointRef {
                        number: 0,
                        record_event: None,
                    }],
                    background_starts: Vec::new(),
                    reported_at: None,
                    message: "DESYNC incomplete callback history for 5126".into(),
                },
            ],
            ..Default::default()
        };
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            value["desync_notes"],
            serde_json::json!([
                {
                    "kind": "callback_checkpoint",
                    "thread": 5125,
                    "record_event": 16559,
                    "checkpoints": [{ "number": 23, "record_event": 16559 }],
                    "reported_at": { "event": 16621, "record_event": 16617 },
                    "message": "DESYNC callback checkpoint 23 for 5125 at record event #16559"
                },
                {
                    "kind": "incomplete_callback_history",
                    "thread": 5126,
                    "record_event": null,
                    "checkpoints": [{ "number": 0, "record_event": null }],
                    "message": "DESYNC incomplete callback history for 5126"
                }
            ])
        );
        let parsed: RunSummary = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(parsed.desync_notes, summary.desync_notes);

        let mut later = value;
        later["desync_notes"][0]["kind"] = serde_json::json!("untaken_background_start");
        let parsed: RunSummary = serde_json::from_value(later).unwrap();
        assert_eq!(parsed.desync_notes[0].kind, DesyncKind::Other);

        let none = serde_json::to_value(RunSummary::default()).unwrap();
        assert!(none.get("desync_notes").is_none(), "{none}");
        let old: RunSummary = serde_json::from_value(none).unwrap();
        assert!(old.desync_notes.is_empty());
    }

    /// `desync_notes` reaches `--summary-json` with each note's kind,
    /// recorded position, background starts and replay position, and reads
    /// back; a summary without notes, or written before the field existed,
    /// omits it; a kind from a later Hermit still parses
    /// (https://github.com/rrnewton/hermit/issues/4876).
    /// The files the rewrite left unchanged for want of mapping symbols are
    /// in the JSON and the INFO view, and absent when there are none.
    #[test]
    fn the_unmarked_rewrite_files_are_stated_in_the_summary() {
        let quiet = RunSummary::default();
        let json = serde_json::to_string(&quiet).unwrap();
        assert!(!json.contains("exclusive_rewrite_"), "{json}");
        assert!(!quiet.info(None).to_string().contains("rewrite-exclusives"));
        let stated = RunSummary {
            exclusive_rewrite_unmarked: vec!["/lib64/libstdc++.so.6".to_owned()],
            ..RunSummary::default()
        };
        let json = serde_json::to_string(&stated).unwrap();
        let back: RunSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.exclusive_rewrite_unmarked,
            stated.exclusive_rewrite_unmarked
        );
        let info = stated.info(None).to_string();
        assert!(info.contains("\n  /lib64/libstdc++.so.6\n"), "{info}");
    }

    #[test]
    fn untaken_background_start_notes_round_trip_through_summary_json() {
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
                checkpoints: Vec::new(),
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
        later["desync_notes"][0]["kind"] = serde_json::json!("a_kind_from_a_later_hermit");
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

    /// A file the in-memory rewrite could not write says why, in the JSON and
    /// beside its counts in both views, so an unprivileged run on btrfs
    /// reports each file as not written and why, not a bare count.
    #[test]
    fn a_file_the_in_memory_rewrite_could_not_write_says_why() {
        let why = "the mapped file cannot be proven to be the one planned";
        let summary = RunSummary {
            exclusive_rewrites: BTreeMap::from([(
                "/lib64/libc.so.6".to_owned(),
                ExclusiveRewrite {
                    rewritten: 0,
                    left: 0,
                    failed: 19,
                    not_written: Some(why.to_owned()),
                },
            )]),
            ..Default::default()
        };
        let line = format!("  /lib64/libc.so.6: 0 rewritten, 0 left, 19 not written ({why})\n");
        assert!(summary.info(None).to_string().contains(&line), "{summary}");
        assert!(summary.to_string().contains(&line), "{summary}");
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            value["exclusive_rewrites"]["/lib64/libc.so.6"]["not_written"],
            why
        );
        let back: RunSummary = serde_json::from_value(value).unwrap();
        assert_eq!(back.exclusive_rewrites, summary.exclusive_rewrites);
    }

    /// The in-memory rewrite's per-path counts are in both views, its plan
    /// cache's peak only in the full report; the peak is left out of the
    /// JSON while zero.
    #[test]
    fn the_in_memory_rewrite_is_in_both_views_and_its_cache_peak_in_the_full_report() {
        let baseline = RunSummary {
            syscalls: Some(7),
            ..Default::default()
        };
        let value = serde_json::to_value(&baseline).unwrap();
        assert!(value.get("exclusive_rewrite_cache_peak").is_none());
        let rewritten = RunSummary {
            exclusive_rewrites: BTreeMap::from([(
                "/lib64/libc.so.6".to_owned(),
                ExclusiveRewrite {
                    rewritten: 19,
                    left: 0,
                    failed: 0,
                    not_written: None,
                },
            )]),
            exclusive_rewrite_cache_peak: 2_254_552,
            ..baseline
        };
        let counts = "  /lib64/libc.so.6: 19 rewritten, 0 left, 0 not written\n";
        let full = format!(
            "Exclusive loops rewritten in the guest's memory, in 1 file(s) (plan cache peak: \
             2254552 bytes):\n{counts}"
        );
        let info =
            format!("Exclusive loops rewritten in the guest's memory, in 1 file(s):\n{counts}");
        assert!(rewritten.to_string().contains(&full));
        assert!(rewritten.info(None).to_string().contains(&info));
        assert!(!rewritten.info(None).to_string().contains("peak"));
        let value = serde_json::to_value(&rewritten).unwrap();
        assert_eq!(value["exclusive_rewrite_cache_peak"], 2_254_552);
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
            "exclusive_pairs_not_rewritten",
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

        // Under the overlay rewrite, the files it was not given are named in
        // both views, as a function of the files and the options.
        let overlay = RunSummary {
            exclusive_pairs_not_rewritten: BTreeMap::from([("/lib64/libc.so.6".to_owned(), 38)]),
            ..pairs.clone()
        };
        let not_rewritten = "Not tried by --rewrite-exclusives, whose overlay rewrites only the \
                             executable and each --rewrite-exclusives-also file (name a file with \
                             --rewrite-exclusives-also to have it try its loops), in 1 file(s):\n  \
                             /lib64/libc.so.6: 38\n";
        assert!(overlay.info(None).to_string().contains(not_rewritten));
        assert!(overlay.to_string().contains(not_rewritten));
        assert!(!pairs.to_string().contains("Not tried"));
        let value = serde_json::to_value(&overlay).unwrap();
        assert_eq!(
            value["exclusive_pairs_not_rewritten"],
            serde_json::json!({"/lib64/libc.so.6": 38})
        );
        assert!(
            serde_json::to_value(&pairs)
                .unwrap()
                .get("exclusive_pairs_not_rewritten")
                .is_none()
        );

        for (counts, line, json) in [
            (
                StoreExclusiveCounts {
                    failed: 20,
                    executed: Some(10_000_020),
                    fully_counted: true,
                    counted_per_mille: None,
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
                    counted_per_mille: None,
                },
                "2 store-exclusives executed, none failed\n",
                serde_json::json!({"failed": 0, "executed": 2, "fully_counted": true}),
            ),
            (
                StoreExclusiveCounts {
                    failed: 0,
                    executed: Some(1),
                    fully_counted: true,
                    counted_per_mille: None,
                },
                "1 store-exclusive executed, none failed\n",
                serde_json::json!({"failed": 0, "executed": 1, "fully_counted": true}),
            ),
            (
                StoreExclusiveCounts {
                    failed: 3,
                    executed: None,
                    fully_counted: true,
                    counted_per_mille: None,
                },
                "3 store-exclusive failures; replay may diverge\n",
                serde_json::json!({"failed": 3, "fully_counted": true}),
            ),
            (
                StoreExclusiveCounts {
                    failed: 0,
                    executed: None,
                    fully_counted: true,
                    counted_per_mille: None,
                },
                "No store-exclusive failed\n",
                serde_json::json!({"failed": 0, "fully_counted": true}),
            ),
            (
                StoreExclusiveCounts {
                    failed: 1,
                    executed: Some(9),
                    fully_counted: false,
                    counted_per_mille: None,
                },
                "1 store-exclusive failure; replay may diverge (9 store-exclusives executed); \
                 not counted for the whole run (the PMU multiplexed the counters, or the task ran \
                 on CPUs whose PMU does not carry these events)\n",
                serde_json::json!({"failed": 1, "executed": 9, "fully_counted": false}),
            ),
            // A count of 0 over most of the run: none observed, not none run.
            (
                StoreExclusiveCounts {
                    failed: 0,
                    executed: Some(0),
                    fully_counted: false,
                    counted_per_mille: Some(997),
                },
                "0 store-exclusives executed, none failed; counted for 99.7% of the run (the PMU \
                 multiplexed the counters, or the task ran on CPUs whose PMU does not carry \
                 these events)\n",
                serde_json::json!({
                    "failed": 0,
                    "executed": 0,
                    "fully_counted": false,
                    "counted_per_mille": 997
                }),
            ),
            // A counter that ran for less than a thousandth of the run.
            (
                StoreExclusiveCounts {
                    failed: 0,
                    executed: Some(0),
                    fully_counted: false,
                    counted_per_mille: Some(0),
                },
                "0 store-exclusives executed, none failed; counted for less than 0.1% of the run \
                 (the PMU multiplexed the counters, or the task ran on CPUs whose PMU does not \
                 carry these events)\n",
                serde_json::json!({
                    "failed": 0,
                    "executed": 0,
                    "fully_counted": false,
                    "counted_per_mille": 0
                }),
            ),
            (
                StoreExclusiveCounts {
                    failed: 2,
                    executed: Some(40),
                    fully_counted: true,
                    counted_per_mille: Some(1000),
                },
                "2 store-exclusive failures; replay may diverge (40 store-exclusives executed)\n",
                serde_json::json!({
                    "failed": 2,
                    "executed": 40,
                    "fully_counted": true,
                    "counted_per_mille": 1000
                }),
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

    /// The binder count is in the full report only: the INFO view that
    /// `--verify` compares leaves it out.
    #[test]
    fn binder_external_calls_show_in_the_report_but_not_the_info_view() {
        let baseline = RunSummary::default();
        let summary = RunSummary {
            binder_external_calls: Some(3),
            ..baseline.clone()
        };
        assert!(
            summary
                .to_string()
                .contains("3 binder call(s) ran as external input")
        );
        assert_eq!(
            summary.info(None).to_string(),
            baseline.info(None).to_string()
        );
        assert!(!baseline.to_string().contains("binder"));
    }
}
