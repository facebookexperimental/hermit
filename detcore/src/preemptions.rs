/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A datatype to abstract a record of thread preemptions, as generated during chaos mode execution.

pub mod boundary_recording;
pub mod callback_history;
mod record_serde;
mod reserved_input;
mod streaming;

use std::collections::BTreeMap;
use std::fs::File;
use std::iter::FromIterator;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use detcore_model::collections::ReplayCursor;
use detcore_model::host_input::HostInputSet;
pub use reserved_input::RecordAdmissionRefusal;
pub(crate) use reserved_input::ReservedInputLayout;
pub use reserved_input::ReservedInputMetadata;
pub use reserved_input::read_recorded_reserved_input;
pub use reserved_input::validate_config_replay_inputs;
use serde::Deserialize;
use serde::Serialize;
use tracing::trace;

use crate::resources::ChaosEpochTransition;
pub use crate::resources::InstructionOffset;
use crate::scheduler::Priority;
use crate::scheduler::runqueue::DEFAULT_PRIORITY;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::scheduler::runqueue::is_ordinary_priority;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::SchedEvent;

/// The first format whose records note every background start
/// (`BackgroundStart`), so that a call such a record does not start at a point
/// did not start there: the next number above [`SWITCH_IN_CHARGE_FORMAT`] on
/// the record-format counter ([`PREEMPTION_RECORD_FORMAT_MAX`]).
///
/// Only a writer that notes every start it takes may state this format or a
/// newer one (`PreemptionWriter::noting_background_starts`). Any other
/// writer, and any record that drops its starts, states at most the format
/// below it ([`format_without_background_starts`]).
pub const BACKGROUND_STARTS_FORMAT: u32 = 4;

/// The newest format a record may state when it does not hold every
/// background start of its run: `format`, lowered below
/// [`BACKGROUND_STARTS_FORMAT`] when it is a format this Hermit reads at or
/// above it (`BACKGROUND_STARTS_FORMAT..=PREEMPTION_RECORD_FORMAT_MAX`). A schedule
/// replay's re-record, a slice-boundary recording, an edit or a shorter
/// prefix of a schedule, and an analyze or bisect trial note no starts, so
/// they keep the timing rules of their format and claim no list of starts.
/// Any other stated format, newer than this Hermit or invalid (0), stays as
/// stated, so that reading the record still refuses it
/// (`PreemptionRecord::validate_format`): lowered, a newer Hermit's record
/// would read as one without what it may hold.
pub fn format_without_background_starts(format: Option<u32>) -> Option<u32> {
    format.map(|format| {
        if (BACKGROUND_STARTS_FORMAT..=PREEMPTION_RECORD_FORMAT_MAX).contains(&format) {
            BACKGROUND_STARTS_FORMAT - 1
        } else {
            format
        }
    })
}

/// The turn that began a kernel-run blocking call
/// (`ResourceID::is_kernel_run_blocking`: a blocking read of an external
/// descriptor, a binder call that waits for another process, a vfork, an
/// `rt_sigsuspend`), committed as BACKGROUND: from then on the call runs in
/// the kernel while other threads' turns commit. The turn runs no guest code
/// and so leaves no event; it is recorded after the global schedule's first
/// `after` events. When a call starts can decide what it sees: a binder read
/// already waiting in the driver is the idle looper a later read on another
/// thread finds, or not. A schedule replay starts the call at the same point,
/// before the event that follows it.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundStart {
    /// How many events of the global schedule came before the turn.
    pub after: u64,
    /// The thread whose call the turn began.
    pub thread: DetTid,
    /// The call's per-thread sequence number (`ExternalOpId::sequence`), as
    /// the readmissions name it (`rejoins`).
    pub call: u64,
}

/// Where a run's rewrite of exclusive loops applied (`RewriteExclusives`).
#[derive(PartialEq, Debug, Eq, Clone, Copy, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteExclusivesMode {
    /// Rewritten copies of the named files, bind-mounted over the
    /// originals before the guest ran (the default namespaced form of
    /// `hermit run --rewrite-exclusives`).
    Overlay,
    /// Every executable file mapping rewritten in the guest's memory as
    /// it was mapped (`Config::rewrite_exclusives_in_memory`).
    Memory,
}

/// The exclusive-loop rewrite a record's run applied: where it applied and
/// the rewrite format that produced the rewritten instructions
/// (`crate::aarch64_lse::REWRITE_FORMAT_VERSION`). The rewrite changes the
/// instructions the guest retires while the files stay the same, so a
/// replay must apply the same rewrite; another mode, or another format
/// version under the same mode, cannot be shown to retire the same
/// instructions and is refused (`rewrite_exclusives_mismatch`).
#[derive(PartialEq, Debug, Eq, Clone, Copy, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewriteExclusives {
    /// Where the rewrite applied.
    pub mode: RewriteExclusivesMode,
    /// The format of the rewriter that produced the rewritten code.
    pub format_version: u32,
}

impl RewriteExclusives {
    /// The rewrite this Hermit applies in `mode`: the current rewriter's
    /// format version.
    pub fn current(mode: RewriteExclusivesMode) -> Self {
        Self {
            mode,
            format_version: crate::aarch64_lse::REWRITE_FORMAT_VERSION,
        }
    }

    /// Whether this rewrite is the in-memory one, as the record's older
    /// `rewrite_exclusives_in_memory` field states it.
    fn in_memory(&self) -> bool {
        self.mode == RewriteExclusivesMode::Memory
    }

    fn describe(&self) -> String {
        let where_ = match self.mode {
            RewriteExclusivesMode::Overlay => {
                "exclusive loops rewritten in overlaid files (--rewrite-exclusives)"
            }
            RewriteExclusivesMode::Memory => {
                "exclusive loops rewritten in the guest's memory (--rewrite-exclusives)"
            }
        };
        format!("{where_} by rewrite format {}", self.format_version)
    }
}

/// A record of all the preemptions and other scheduling events that occur during execution.
#[derive(PartialEq, Default, Debug, Eq, Clone, Hash)]
pub struct PreemptionRecord {
    // TODO(T106838933): switch the keys from DetTid to Pedigree.
    /// A sorted list of end-of-timeslice preemption times for each thread.
    per_thread: BTreeMap<DetTid, ThreadHistory>,
    global: Vec<SchedEvent>,
    /// The virtual-time epoch of the run that produced this record.
    ///
    /// Every `LogicalTime` above is an absolute virtual instant, i.e. an offset
    /// from this epoch, so the record only describes a run whose clock starts
    /// here. A replay started from any other epoch would see every recorded
    /// timeslice end shifted by the difference. `None` for records written
    /// before the epoch was stored, and for records synthesized in memory.
    epoch: Option<DateTime<Utc>>,
    /// The `--pipe-wake-latency` of the run that produced this record
    /// (<https://github.com/rrnewton/hermit/issues/4380>). A hand-off moves
    /// turns and their charged time, so a replay under another value cannot
    /// reproduce the run, and is refused. `None` when the option was off,
    /// which writes the record exactly as before the field existed, and for
    /// records written before it. `Some(0)` is distinct from `None`.
    pipe_wake_latency: Option<u64>,
    /// Whether Detcore's signal-state copy was on in the run that produced
    /// this record (`Config::signal_state_copy`, A5 step 2d). With it on,
    /// some signal decisions read the copy, and ptrace reports each handler
    /// return, which can move a preemption in the cases docs/SIGNAL_STATE_CHECK.md
    /// lists, so a replay under the other setting is refused. `None` for
    /// records written before the field existed, and for records synthesized
    /// in memory, which are replayed under either setting.
    signal_state_copy: Option<bool>,
    /// Whether the run that produced this record charged preemptions at most
    /// the time their slice ran (`--cap-preemption-charge`). Turn charges
    /// move timers and timeouts, so a replay under the other setting cannot
    /// reproduce the run, and is refused. Off writes the record exactly as
    /// before the field existed.
    cap_preemption_charge: bool,
    /// The `--max-timeslice` the run that produced this record drew under
    /// `--chaos-timeslice-draw=run` (`Config::chaos_timeslice`), in virtual
    /// nanoseconds. A replay of the record runs under it (`hermit run` takes
    /// it unless the replay gives another `--max-timeslice`, which is
    /// refused). `None`, which writes nothing, for a run that drew none and
    /// for records written before the field existed.
    chaos_timeslice: Option<u64>,
    /// How the run that produced this record numbered its guest's process
    /// and thread IDs (`Config::guest_ids`), when no PID namespace of its own
    /// did (`--no-namespace`). Every thread above is keyed by those IDs, so a
    /// replay under another numbering is refused. `None` for a run its PID
    /// namespace numbered, which writes the record exactly as before the
    /// field existed, and for records written before it.
    guest_ids: Option<detcore_model::pid::GuestIds>,
    /// Set in a record written by a preemption replay that left a change point
    /// (and its slowdown-epoch transitions) out of it because it would not
    /// have come after the thread's last one: the global event count where it
    /// first did. A record's rows must increase (`Self::validate`), so such a
    /// change point cannot be written, and the record no longer holds the
    /// priority the thread had at that instant: it is not an exact record of
    /// the run, and faithful replay refuses it. That happens to a replay that left the
    /// record it replays, and to a replay of an edited record
    /// (`--replay-edited-preemptions`), which keeps a slice that ends at once.
    /// A raw schedule replay that records past the end of its schedule under
    /// `--chaos` or another scheduling heuristic sets it too, at the global
    /// event count where the schedule ran out or was released: the record
    /// holds none of the continuation's priority change points and keeps the
    /// replayed schedule's charging rules, so it is not an exact record of the
    /// run either (`Scheduler::release_replay_priorities_if_exhausted`). The
    /// mark is applied only once the run continues past that point; a run
    /// that ends with its schedule omitted nothing and stays unmarked.
    /// Absence preserves legacy admission; it does not prove historical
    /// completeness. Newly marked JSON uses a mandatory envelope that old
    /// readers reject, and marked non-human serialization is refused.
    replay_departed_at: Option<u64>,
    /// The actual private-input layout of the producing target images. This
    /// standalone JSON member is deliberately absent from ThreadHistory/RPC.
    /// Unknown values are retained for editing and non-Chosen replay.
    reserved_input: ReservedInputMetadata,
    /// The post-crash bound in effect in the run that produced this record
    /// (`Config::post_crash_timeslice_in_effect`), in virtual nanoseconds. The
    /// switch changes every slice after it, so a replay takes this value
    /// (`post_crash_timeslice_for_replay`) and refuses another. `None` when
    /// the switch was off, which writes the record exactly as before the
    /// field existed, and for records written before it.
    post_crash_timeslice: Option<u64>,
    /// Where the run that produced this record took the post-crash switch,
    /// each time it did, in order (`crate::post_crash`). A replay that
    /// follows the record takes the same switches at the same points, and is
    /// refused at a different one.
    /// Present (possibly empty) only in a record its own run wrote with the
    /// switch on, so a replay can tell "no switch was taken" from a record
    /// derived from another, which holds none (`None`).
    post_crash: Option<Vec<crate::post_crash::PostCrashSwitch>>,
    /// Actual callback expirations associated with existing priority-history rows.
    /// Absent in old/edited recordings; never inferred from time or branch counts.
    callback_checkpoints: BTreeMap<DetTid, Vec<callback_history::Checkpoint>>,
    /// The `--rng-seed` of the run that produced this record.
    ///
    /// It decides what the guest reads as randomness, so the recorded schedule
    /// only describes a guest that read the same values: `--seed` sets it too, so
    /// a replay at another `--seed` would otherwise hand the guest other values.
    /// `None` for records written before the field existed, and for records
    /// synthesized in memory.
    rng_seed: Option<u64>,
    /// Whether the run that produced this record charged a run of
    /// consecutive expired timed waits one scheduler turn
    /// (`Scheduler::expired_wait_runs_charged_once`): a run under
    /// `--expired-wait-charge=once`, by default one outside race search.
    /// `false`, which writes the record exactly as before the field existed
    /// and is what a record without it means, is the rule of every run
    /// before it and of `each` runs: each expired wait is charged. A replay
    /// that leaves `--expired-wait-charge` unset charges as its record says,
    /// so that a chaos recording replayed without `--chaos` (or a plain one
    /// replayed with it) keeps its recorded virtual times.
    expired_wait_runs_charged_once: bool,
    /// The host files the recorded run's guest opened
    /// (`detcore_model::host_input::HostInputSet`), so that a replay can
    /// report one that changed since (`HERMIT_HOST_INPUT_CHANGED_SINCE_RECORDING`).
    /// Information only: a replay never refuses or alters its schedule for
    /// it. `None` for records written before the field existed, and for
    /// records synthesized in memory.
    host_inputs: Option<HostInputSet>,
    /// Whether the run that produced this record registered a queued plain
    /// sleep as a timed waiter at the first scheduler pass that found it
    /// queued (`Scheduler::queued_sleeps_registered_at_first_pass`): a run
    /// under `--sleep-registration=first-pass`, by default one outside race
    /// search. `false`, which writes the record exactly as before the field
    /// existed and is what a record without it means, is the rule of every
    /// run before it and of `when-chosen` runs: a sleep is registered only
    /// when its thread is chosen before its deadline. A replay that leaves
    /// `--sleep-registration` unset registers as its record says.
    queued_sleeps_registered_at_first_pass: bool,
    /// The store-exclusives that failed in the run that produced this record
    /// (aarch64, `STREX_FAIL_SPEC`). Each retried a load/store exclusive loop
    /// and so added retired branches the run's preemption points count, so a
    /// replay of this record may diverge; a replay warns at its start when it
    /// is nonzero. Zero, and absent, for a run that counted none, for other
    /// architectures, and for records written before the field existed.
    store_exclusive_failures: HostObservedCount,
    /// The record's format ([`PREEMPTION_RECORD_FORMAT_MAX`]): `None` for format 1.
    format_version: Option<u32>,
    /// Every jump of global time to a deadline that the scheduler made because
    /// nothing else could run (`TimeSkip`), in order; absent when there was
    /// none. A record of format 2 or later lists all its jumps, so a replay of
    /// it makes exactly those jumps, each where the recording made it
    /// (`TimeSkip::event`), and computes none of its own. A record of format 1
    /// (written before jumps were recorded) replays as before: it makes no
    /// idle-poller jump, and a FIFO open's fast-forward recomputes its jump.
    /// Analyze and bisect carry a source record's jumps into the trial
    /// schedules they splice from it (`with_time_skips`).
    time_skips: Option<Vec<TimeSkip>>,
    /// Where each backgrounded call rejoined the run queue in the run that
    /// produced this record (`crate::scheduler::rejoin_log`).
    ///
    /// A call on the background path (`BlockingExternalIO`: a blocking read
    /// of an external descriptor, a binder call that waits for another
    /// process) runs in the kernel while guest turns continue, and the
    /// recording readmits it at the first scheduler pass that finds it
    /// finished. That turn is host-timed, so a replay that readmits "when
    /// finished" can leave the schedule it replays. With these entries the
    /// replay readmits each call at the turn the recording did. A record a
    /// run writes always has them, empty when no call went to the background
    /// (`PreemptionWriter::new`); one written before the field existed, and
    /// one synthesized or edited in memory, has `None`. Either kind with no
    /// entry replays as before.
    rejoins: Option<Vec<crate::scheduler::rejoin_log::Rejoin>>,
    /// The turns that began kernel-run blocking calls, in the order they
    /// committed (`BackgroundStart`). Empty for records of format 1, which
    /// replay as before, for runs that began no such call, and for records
    /// synthesized in memory; dropped, with the readmissions, by any edit of
    /// the schedule.
    background_starts: Vec<BackgroundStart>,
    /// Whether the run that produced this record rewrote exclusive loops in
    /// the guest's memory (`Config::rewrite_exclusives_in_memory`, aarch64).
    /// The rewrite changes the instructions the guest retires while every
    /// file stays the same, so a replay under the other setting would run
    /// other code with nothing to tell it apart, and is refused. Written only
    /// when on, so absent means off, as for every record made before it.
    /// Kept beside `rewrite_exclusives` so a reader that predates the
    /// provenance still refuses the in-memory mismatch it can see; the two
    /// never disagree in a record this Hermit writes.
    rewrite_exclusives_in_memory: bool,
    /// Whether the record holds rows alone, without the schedule trace and
    /// jumps of global time of the run they came from: a copy of a record's
    /// rows (`clone_preemptions_only`) whose rows hold an instruction offset.
    /// Such a record states [`INSTRUCTION_OFFSET_ROWS_FORMAT`] to fence out a
    /// Hermit that predates the offsets, and follows format 1's timing rules
    /// (`timing_format`). A record whose schedule trace is merely empty, a
    /// prefix of no events or a run that recorded none, is not rows alone: it
    /// keeps its format's timing. Written only when set.
    rows_alone: bool,
    /// Whether the record notes none of its run's background starts although
    /// it states a format at or above [`BACKGROUND_STARTS_FORMAT`], whose
    /// number alone would say it notes them all (`notes_background_starts`).
    /// Set where a record without its starts (a shorter prefix, an edit, a
    /// writer that notes none) holds instruction offsets, which keep it at
    /// [`INSTRUCTION_OFFSET_ROWS_FORMAT`] (`forget_background_starts`,
    /// `raise_format`). A record without offsets goes below the starts'
    /// format instead and never has it, so a record main writes is unchanged.
    /// Written only when set.
    background_starts_unnoted: bool,
    /// The exclusive-loop rewrite the record's run applied, if any
    /// (`RewriteExclusives`): its mode and rewrite format version. `None`
    /// in a record whose run rewrote nothing and in every record made
    /// before the field existed — including an in-memory one whose
    /// `rewrite_exclusives_in_memory` is set — whose rewritten
    /// instructions no replay that rewrites can be shown to reproduce,
    /// and which is therefore refused for such a replay.
    rewrite_exclusives: Option<RewriteExclusives>,
}

/// A count a record's run observed on its host rather than decided: it is
/// kept in the record but compares and hashes as equal whatever its value,
/// so that records of two runs of one schedule stay equal.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
struct HostObservedCount(u64);

impl HostObservedCount {
    fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

impl PartialEq for HostObservedCount {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for HostObservedCount {}

impl std::hash::Hash for HostObservedCount {
    fn hash<H: std::hash::Hasher>(&self, _: &mut H) {}
}

/// The newest preemption-record format this Hermit writes and reads
/// (`PreemptionRecord::format_version`), one counter for every format change:
/// a record of format N has every property of the formats before it.
///
/// - Format 1 has no version field: every record written before the field
///   existed.
/// - Format 2 ([`TIME_SKIPS_FORMAT`]) lists every jump of global time its run
///   made (`time_skips`, absent when there was none).
/// - Format 3 ([`SWITCH_IN_CHARGE_FORMAT`]) was written by a run that charged
///   every stretch of guest code a turn, so its replay charges a replayed
///   switch-in under which guest code runs.
/// - Format 4 ([`BACKGROUND_STARTS_FORMAT`]) notes every background start its
///   run took (`background_starts`), so its schedule replay begins each
///   kernel-run call where the recording did.
/// - Format 5 ([`STOPLESS_SLICE_ENDS_FORMAT`]) may mark a Branch event
///   `stopless`: a lone thread's timeslice end its run took without a stop,
///   so the event has no `end_rip`, and a replay takes that end without a stop
///   where its thread runs alone.
/// - Format 6 ([`INSTRUCTION_OFFSET_ROWS_FORMAT`]) may hold
///   `--chaos-instruction-offset` decisions in its rows. Rows alone
///   (`PreemptionRecord::rows_alone`) follow format 1's timing rules
///   (`PreemptionRecord::timing_format`).
///
/// A record that drops one feature's data while it keeps a later feature's
/// follows one rule. A feature that can be lowered without breaking replay
/// is lowered, its data cleared with it: a record that notes no background
/// starts holds no `stopless` marks
/// (`PreemptionRecord::forget_stopless_marks_below_their_format`). One that
/// cannot be lowered keeps its number and marks the earlier feature's data
/// absent with an explicit flag instead of clearing it: rows with
/// instruction offsets keep format 6 and say that they note no background
/// starts (`PreemptionRecord::background_starts_unnoted`), as rows alone say
/// they hold no schedule trace (`PreemptionRecord::rows_alone`). So a reader
/// decides what a record holds by its format and those flags together
/// (`PreemptionRecord::notes_background_starts`,
/// `PreemptionRecord::timing_format`), never by the number alone, and a
/// Hermit that reads only older formats still refuses the record by its
/// number.
///
/// A record of a newer format is refused (`PreemptionRecord::validate_format`),
/// so a Hermit that reads only formats 1 to 4 refuses a record with such
/// marks rather than replay it with a stop it would not find at those ends, and
/// one that reads only formats 1 to 5 refuses a record with offset rows.
pub const PREEMPTION_RECORD_FORMAT_MAX: u32 = INSTRUCTION_OFFSET_ROWS_FORMAT;

/// The format a fresh record states when its rows hold no instruction
/// offset: the newest format below [`INSTRUCTION_OFFSET_ROWS_FORMAT`],
/// which is [`STOPLESS_SLICE_ENDS_FORMAT`]. Such a file is written exactly
/// as before the offsets existed, so every Hermit still reads it; only a
/// record whose rows hold an offset states
/// [`PREEMPTION_RECORD_FORMAT_MAX`]. The writer stamps this format
/// (`format_without_instruction_offsets`), and its first row with an
/// offset raises it (`PreemptionWriter::insert_reprioritization`).
pub const PREEMPTION_RECORD_FORMAT_WITHOUT_OFFSETS: u32 = INSTRUCTION_OFFSET_ROWS_FORMAT - 1;

/// The format from which a record lists all its jumps of global time.
pub const TIME_SKIPS_FORMAT: u32 = 2;

/// The format from which a schedule replay charges a scheduler turn for every
/// replayed switch to a thread that then runs guest code
/// (`Scheduler::bump_global_time`), as the run that wrote the record charged
/// the yield turn the switch stands for. A run charges every such stretch when
/// it follows no record, or follows one of this format. A replay of an older
/// record charges as before: its writer cannot say whether its own run
/// followed a schedule, and so left those stretches uncharged.
///
/// This restores the switch-in charge when guest events retire. A bare
/// `TraceReplay` turn does not become `previous_turn_tid`: main's ordinary
/// turn rule continues to decide subsequent signal charges. This format alone
/// does not claim equality of every record/replay clock observation.
pub const SWITCH_IN_CHARGE_FORMAT: u32 = 3;

/// The format from which a record may mark a lone thread's timeslice end
/// taken without a stop (`SchedEvent::stopless`): the next number above
/// [`BACKGROUND_STARTS_FORMAT`]. A writer that notes its run's background
/// starts writes it (`PreemptionWriter::noting_background_starts`): a run
/// that follows no record, and a preemption replay's re-record. A record of
/// an older format holds no mark: a writer below this format records such an
/// end as an ordinary one (`PreemptionWriter::insert_schedevent`), and a
/// record lowered below `BACKGROUND_STARTS_FORMAT` drops its marks with its
/// starts (`PreemptionRecord::forget_stopless_marks_below_their_format`). A
/// record read with a mark in an older format is refused
/// (`PreemptionRecord::validate_format`).
pub const STOPLESS_SLICE_ENDS_FORMAT: u32 = 5;

/// The format of a record whose rows hold a `--chaos-instruction-offset`
/// decision (`ThreadHistory::preemption_instruction_offsets`): the next number
/// above [`STOPLESS_SLICE_ENDS_FORMAT`] on the record-format counter.
///
/// It is the record's mandatory compatibility boundary. A Hermit that
/// predates the offsets reads a thread's rows without them, so it would stop
/// each such slice at its branch target rather than the recorded instruction;
/// it reads formats 1 to 5 only and refuses this one by name. So a record
/// states it as soon as its rows hold one offset (`raise_format`, which the
/// writer calls at the first, and every edit or copy that keeps such a row
/// keeps), and validation refuses offset rows in a record of an older format
/// (`PreemptionRecord::validate_format`). A record whose rows hold none is
/// written in the newest format below it
/// ([`format_without_instruction_offsets`]), readable as before.
///
/// A record of this format has every timing property of
/// [`SWITCH_IN_CHARGE_FORMAT`], even with an empty schedule trace, as a
/// prefix of no events has. Rows alone, copied from a record without its
/// schedule trace and jumps of global time
/// (`PreemptionRecord::clone_preemptions_only`), say so
/// (`PreemptionRecord::rows_alone`) and follow format 1's timing rules, as
/// the same rows without offsets state no format: they list no jumps of
/// global time and charge no replayed switch-in. Every reader that keys
/// timing on the format reads `PreemptionRecord::timing_format`, which
/// decides that by the mark, not the number alone.
pub const INSTRUCTION_OFFSET_ROWS_FORMAT: u32 = 6;

/// The newest format a record may state when its rows hold no instruction
/// offset: `format`, lowered below [`INSTRUCTION_OFFSET_ROWS_FORMAT`] when it is a
/// format this Hermit reads at or above it
/// (`INSTRUCTION_OFFSET_ROWS_FORMAT..=PREEMPTION_RECORD_FORMAT_MAX`). A run without
/// `--chaos-instruction-offset` decisions keeps the timing rules of its format
/// and claims no offsets. Any other stated format, newer than this Hermit or
/// invalid (0), stays as stated, so that reading the record still refuses it
/// (`PreemptionRecord::validate_format`): lowered, a newer Hermit's record
/// would read as one without what it may hold.
pub fn format_without_instruction_offsets(format: Option<u32>) -> Option<u32> {
    format.map(|format| {
        if (INSTRUCTION_OFFSET_ROWS_FORMAT..=PREEMPTION_RECORD_FORMAT_MAX).contains(&format) {
            PREEMPTION_RECORD_FORMAT_WITHOUT_OFFSETS
        } else {
            format
        }
    })
}

/// Whether a record that follows the timing rules of `timing` (`None` for
/// format 1; `PreemptionRecord::timing_format`) lists every jump of global
/// time its run made: format 2 ([`TIME_SKIPS_FORMAT`]) or later.
pub(crate) fn lists_time_skips_in(timing: Option<u32>) -> bool {
    timing.is_some_and(|format| format >= TIME_SKIPS_FORMAT)
}

/// Whether a schedule replay of a record that follows the timing rules of
/// `timing` charges a replayed switch-in under which guest code runs: format
/// 3 ([`SWITCH_IN_CHARGE_FORMAT`]) or later.
pub(crate) fn charges_replayed_switch_ins_in(timing: Option<u32>) -> bool {
    timing.is_some_and(|format| format >= SWITCH_IN_CHARGE_FORMAT)
}

/// Whether this Hermit reads a record stating `format` (`None` for format 1):
/// format 1, written without the field, or one from [`TIME_SKIPS_FORMAT`] to
/// [`PREEMPTION_RECORD_FORMAT_MAX`].
pub(crate) fn reads_format(format: Option<u32>) -> bool {
    format.is_none_or(|format| (TIME_SKIPS_FORMAT..=PREEMPTION_RECORD_FORMAT_MAX).contains(&format))
}

/// A record whose format, or jumps of global time, this Hermit refuses to
/// replay (`PreemptionRecord::validate_format`): a policy refusal, not a
/// malformed file.
#[derive(Debug)]
pub struct RecordFormatRefusal(pub String);

impl std::fmt::Display for RecordFormatRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RecordFormatRefusal {}

/// Whether `PreemptionRecord::time_skips` has nothing to write.
fn lists_no_time_skips(skips: &Option<Vec<TimeSkip>>) -> bool {
    skips.as_ref().is_none_or(Vec::is_empty)
}

/// Why the scheduler moved global time ahead to a deadline.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Hash, Serialize, Deserialize)]
pub enum TimeSkipReason {
    /// A FIFO open waits for a peer while every other thread is an idle
    /// guest-state poller or parked.
    FifoOpenAlone,
    /// A guest-state poller retried without progress while every other thread
    /// is an idle guest-state poller or parked.
    IdleGuestPollers,
}

/// One jump of global time. A replay of the record makes it where the
/// recording did, as replay drives context switches, and checks it: its target
/// must still lie ahead, and no finite deadline the replay knows, then or
/// later, may come before it, or the replay reports a desync
/// (`Scheduler::make_recorded_time_skip`).
#[derive(PartialEq, Debug, Eq, Clone, Copy, Hash, Serialize, Deserialize)]
pub struct TimeSkip {
    /// Where in the recorded stream the jump falls: the number of stored
    /// records before it (global events in the original format, and every
    /// record, boundaries included, in the boundary protocol). Taken where the
    /// jump is made, at the start of a scheduler pass, when every thread is
    /// parked, so no event can race it.
    pub event: u64,
    /// The exact deadline global time moved to.
    pub target: LogicalTime,
    /// Why nothing else could run.
    pub reason: TimeSkipReason,
}

/// Read only the readmissions stored in a preemption record file, as
/// [`read_recorded_rng_seed`] reads the guest random seed, whatever the
/// record's format, as every loader keeps them
/// (`PreemptionRecord::validate_contents`). `Some` list, possibly empty,
/// comes from a record whose run logged its readmissions;
/// `None` means the record predates the field, was synthesized or edited in
/// memory, or cannot be read at all: a replay that cannot read its record is
/// refused by the reader that needs the whole record, with a message about
/// the record rather than about one field. A record whose readmissions are
/// out of order is refused here, because replay walks them once and could
/// not follow them.
pub(crate) fn read_recorded_rejoins(
    path: &Path,
) -> Result<Option<Vec<crate::scheduler::rejoin_log::Rejoin>>, String> {
    #[derive(Deserialize)]
    struct RejoinsOnly {
        #[serde(default)]
        rejoins: Option<Vec<crate::scheduler::rejoin_log::Rejoin>>,
    }
    let Ok(read) = boundary_recording::read_settings::<RejoinsOnly>(path) else {
        return Ok(None);
    };
    if let Some(rejoins) = &read.rejoins {
        crate::scheduler::rejoin_log::ordered(rejoins)
            .map_err(|why| format!("{}: {why}", path.display()))?;
    }
    Ok(read.rejoins)
}

impl std::fmt::Display for PreemptionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        streaming::fmt_json(self, f, false)
    }
}

impl PreemptionRecord {
    /// TODO: This is not ideal because PreemptionRecord shouldn't be doing double duty as an eventlog.
    pub fn from_sched_events(events: Vec<SchedEvent>) -> Self {
        Self {
            per_thread: Default::default(),
            global: events,
            epoch: None,
            pipe_wake_latency: None,
            signal_state_copy: None,
            cap_preemption_charge: false,
            chaos_timeslice: None,
            guest_ids: None,
            replay_departed_at: None,
            reserved_input: ReservedInputMetadata::default(),
            post_crash_timeslice: None,
            post_crash: None,
            callback_checkpoints: BTreeMap::new(),
            rng_seed: None,
            expired_wait_runs_charged_once: false,
            host_inputs: None,
            store_exclusive_failures: HostObservedCount::default(),
            format_version: None,
            time_skips: None,
            queued_sleeps_registered_at_first_pass: false,
            rejoins: None,
            background_starts: Vec::new(),
            rewrite_exclusives_in_memory: false,
            rows_alone: false,
            background_starts_unnoted: false,
            rewrite_exclusives: None,
        }
    }

    /// Whether the run this record came from charged a run of expired timed
    /// waits one turn (`false` for each wait charged, and for records that
    /// do not say).
    pub fn expired_wait_runs_charged_once(&self) -> bool {
        self.expired_wait_runs_charged_once
    }

    /// The same record, stating how the run it came from charged runs of
    /// expired timed waits.
    pub fn with_expired_wait_runs_charged_once(mut self, once: bool) -> Self {
        self.expired_wait_runs_charged_once = once;
        self
    }

    /// The record a replay of only the first `len` scheduled events of this
    /// one follows: those events, and the callback checkpoints they reach.
    ///
    /// A checkpoint names the callback that ended one of a thread's slices
    /// and the thread's progress there (the sum of its events' counts, as
    /// replay counts it, [`callback_history::Progress`]). The checkpoints of
    /// threads the prefix runs, up to the progress each thread reaches in it,
    /// end the prefix's slices where the recording did, so a replay of the
    /// prefix keeps the software-expiry turns they mark. A schedule replayed
    /// without them has no record of those turns, and its virtual time drifts
    /// from the recording's. The scheduler runs freely after the prefix, so
    /// later checkpoints are dropped, and so are those of threads the prefix
    /// does not run, progress-0 ones included.
    ///
    /// Progress alone places a checkpoint, so one taken at exactly the
    /// progress a thread reaches at the prefix end is kept: it ended the
    /// thread's last slice in the prefix. Another one at the same progress,
    /// from a later callback of the same handler after other threads ran past
    /// the prefix, is kept too, and the replay applies it only when its
    /// recorded time and branch count are reached.
    ///
    /// The thread that runs the prefix's last event is the exception while
    /// another event follows: its checkpoints at the progress that event
    /// reaches end its slice at the prefix's own end, which decides what runs
    /// next, so they belong to the event after the prefix and are dropped. A
    /// replay of the prefix then lets that thread run on, as it does past any
    /// event the prefix leaves out. A prefix of the whole schedule keeps the
    /// record's checkpoints.
    ///
    /// The per-thread histories stay whole, for the rows the checkpoints
    /// name; a schedule replay reads only the events and the checkpoints.
    pub fn schedule_prefix(&self, len: usize) -> PreemptionRecord {
        let PreemptionRecord {
            per_thread,
            global,
            epoch,
            pipe_wake_latency,
            signal_state_copy,
            cap_preemption_charge,
            chaos_timeslice,
            guest_ids,
            reserved_input,
            post_crash_timeslice,
            post_crash,
            callback_checkpoints,
            rng_seed,
            expired_wait_runs_charged_once,
            host_inputs,
            store_exclusive_failures: _,
            replay_departed_at,
            format_version,
            time_skips,
            queued_sleeps_registered_at_first_pass,
            rejoins,
            background_starts,
            rewrite_exclusives_in_memory,
            rows_alone,
            background_starts_unnoted,
            rewrite_exclusives,
        } = self;
        let followed = len < global.len();
        let global = global[..len.min(global.len())].to_vec();
        let mut reached = BTreeMap::<DetTid, callback_history::Progress>::new();
        for event in &global {
            reached
                .entry(event.dettid)
                .or_default()
                .observe_event(event);
        }
        // The thread whose slice is still running at the prefix's end: the
        // last event's, when that event advances it and another one follows.
        let running = global
            .last()
            .filter(|last| followed && last.count > 0)
            .map(|last| last.dettid);
        let callback_checkpoints = callback_checkpoints
            .iter()
            .filter_map(|(tid, points)| {
                let reached = reached.get(tid)?.count;
                let within = |progress: u64| {
                    if running == Some(*tid) {
                        progress < reached
                    } else {
                        progress <= reached
                    }
                };
                let kept: Vec<_> = points
                    .iter()
                    .filter(|point| within(point.origin.progress))
                    .cloned()
                    .collect();
                (!kept.is_empty()).then_some((*tid, kept))
            })
            .collect();
        let mut prefix = PreemptionRecord {
            per_thread: per_thread.clone(),
            global,
            epoch: *epoch,
            pipe_wake_latency: *pipe_wake_latency,
            signal_state_copy: *signal_state_copy,
            cap_preemption_charge: *cap_preemption_charge,
            chaos_timeslice: *chaos_timeslice,
            guest_ids: *guest_ids,
            reserved_input: reserved_input.clone(),
            post_crash_timeslice: *post_crash_timeslice,
            // The switches the run took name points of the recorded run, as
            // the checkpoints do. A prefix of the whole schedule keeps them;
            // a shorter one lists none and is not checked against them, as a
            // record derived from another (`clone_preemptions_only`) is not.
            post_crash: if followed { None } else { post_crash.clone() },
            callback_checkpoints,
            rng_seed: *rng_seed,
            expired_wait_runs_charged_once: *expired_wait_runs_charged_once,
            host_inputs: host_inputs.clone(),
            // A count of this run's host, not part of the preemptions.
            store_exclusive_failures: HostObservedCount::default(),
            // A prefix of an inexact record is inexact too.
            replay_departed_at: *replay_departed_at,
            // The jumps the prefix reaches, in stream order, as analyze and
            // bisect carry them (`hermit-cli`'s `time_skip_carry`): one at its
            // end still waits there for a FIFO open's poll, and one beyond it
            // was never reached.
            time_skips: time_skips
                .as_ref()
                .map(|skips| {
                    skips
                        .iter()
                        .take_while(|skip| {
                            usize::try_from(skip.event).is_ok_and(|event| event <= len)
                        })
                        .copied()
                        .collect::<Vec<_>>()
                })
                .filter(|skips| !skips.is_empty()),
            queued_sleeps_registered_at_first_pass: *queued_sleeps_registered_at_first_pass,
            // The readmissions name points of the recorded run, as the
            // checkpoints do. A prefix of the whole schedule keeps them; a
            // shorter one drops them, as an edited record does
            // (`forget_run_points`), so its replay readmits a finished call
            // when it finds it finished.
            rejoins: if followed { None } else { rejoins.clone() },
            format_version: *format_version,
            background_starts: background_starts.clone(),
            // A setting of the recorded run, which a replay must share.
            rewrite_exclusives_in_memory: *rewrite_exclusives_in_memory,
            rows_alone: *rows_alone,
            background_starts_unnoted: *background_starts_unnoted,
            rewrite_exclusives: *rewrite_exclusives,
        };
        // The background starts are points of the recorded run too, and go
        // the same way, as an edited record's do (`forget_run_points`): a
        // record that notes background starts says it holds every start its
        // run took, so a shorter prefix that kept that claim without them
        // would tell its replay that no call began. It keeps the timing rules
        // of its format (`forget_background_starts`).
        if followed {
            prefix.forget_background_starts();
        }
        prefix.forget_stopless_marks_below_their_format();
        prefix
    }

    /// Drop the `stopless` marks of a record whose format is older than
    /// [`STOPLESS_SLICE_ENDS_FORMAT`], or that says it notes no background
    /// starts (`background_starts_unnoted`), so that it reads back
    /// (`validate_format`): each such end then replays with a stop. This is
    /// how a record that keeps no background starts, lowered below
    /// [`BACKGROUND_STARTS_FORMAT`] (`format_without_background_starts`) or
    /// kept at the format of its offset rows with the flag
    /// (`forget_background_starts`), keeps its lone-slice ends. It follows
    /// the format ladder's rule: a later feature that cannot be lowered
    /// keeps its number and marks the earlier feature's data absent with
    /// an explicit flag; a feature that can be lowered without breaking
    /// replay is lowered. These marks can be, where the end stays visible
    /// without the mark: a replay that stops at such an end reproduces
    /// the run as one that takes it without a stop does (`hermit-cli`'s
    /// `deadline_slice_cap` tests compare a recording's replays with
    /// those of a recording that stopped at every end), because an end a
    /// following Branch or another thread's event marks is found by the
    /// replay's own rule (`Replayer::timer_preemption_branches`).
    ///
    /// A mark whose end would become invisible is kept, not dropped: the
    /// guest retired its last branch at that end and reached a check-in
    /// of the same thread with no branch between, which a replay reads
    /// as branches that reach a check-in, not as a slice end. Clearing
    /// such a mark would silently remove the slice end, its grant and
    /// its virtual-time charge, so the lowered record instead keeps the
    /// mark and is refused wherever records are validated or read
    /// (`validate_format`: such ends need format
    /// [`STOPLESS_SLICE_ENDS_FORMAT`] or later; `validate_contents`:
    /// only a record that notes its starts holds them). The conversion
    /// is refused, never lossy.
    fn forget_stopless_marks_below_their_format(&mut self) {
        if self.background_starts_unnoted
            || self
                .format_version
                .is_none_or(|format| format < STOPLESS_SLICE_ENDS_FORMAT)
        {
            for index in 0..self.global.len() {
                let event = &self.global[index];
                if !event.stopless {
                    continue;
                }
                // The last event's end is the record's end: no following
                // turn is decided by it, so clearing its mark loses no
                // endpoint a replay could take.
                let end_stays_visible = self.global.get(index + 1).is_none_or(|following| {
                    following.op == detcore_model::schedule::Op::Branch
                        || following.dettid != event.dettid
                });
                if end_stays_visible {
                    self.global[index].stopless = false;
                }
            }
        }
    }

    /// The refusal for a record whose `stopless` marks it may not hold, or
    /// `None` if it may: after lowering, a remaining mark is one whose
    /// slice end would have become invisible without it, so the record is
    /// refused rather than lossy (`validate_format` and
    /// `validate_contents` state the same refusals on read).
    fn stopless_lowering_refusal(&self) -> Result<(), String> {
        let stopless = self.global.iter().filter(|event| event.stopless).count();
        if stopless > 0 && self.background_starts_unnoted {
            Err(format!(
                "the preemption record says it notes no background starts \
                 (background_starts_unnoted) but marks {stopless} timeslice ends taken \
                 without a stop (`stopless`), which only a record that notes its starts holds"
            ))
        } else if stopless > 0
            && self
                .format_version
                .is_none_or(|format| format < STOPLESS_SLICE_ENDS_FORMAT)
        {
            Err(format!(
                "record format {:?} with {stopless} timeslice ends taken without a stop \
                 (`stopless`): such ends need format {STOPLESS_SLICE_ENDS_FORMAT} or later",
                self.format_version
            ))
        } else {
            Ok(())
        }
    }

    /// The same record with `events` as its schedule, or `None` if its
    /// callback checkpoints do not fit those events (see [`Self::validate`]).
    /// For a schedule that replayed this record's events and went on past
    /// them: its own recording keeps no checkpoints, since a replay records
    /// none, but the replayed part ended its slices where this record did.
    /// Also for a schedule that starts with this record's events and goes on
    /// with others (a search's edit after a [`Self::schedule_prefix`]): a
    /// checkpoint the edited part does not reach exactly (callback origin,
    /// time and branch count) is reported as a desync and otherwise ignored.
    pub fn with_schedule(&self, events: Vec<SchedEvent>) -> Option<PreemptionRecord> {
        let mut record = self.clone();
        record.global = events;
        record.forget_stopless_marks_below_their_format();
        record.validate().is_ok().then_some(record)
    }

    /// Attach the metadata of two interpolation inputs to `events`.
    ///
    /// The caller first reconciles the existing guest-ID, signal-copy and
    /// other option policies. An exact source schedule keeps that source's
    /// complete record, including callback checkpoints and time jumps. Both
    /// formats must agree: a single output cannot follow two charging rules.
    ///
    /// A genuinely mixed order has no established mapping for either source's
    /// jumps or checkpoints, so refuse those inputs. For jump/checkpoint-free
    /// inputs, every remaining metadata field must agree. Keep those fields,
    /// but clear the per-thread priority histories, as an event-order edit
    /// follows its global stream rather than either original priority history.
    /// Host-observed failure counts must agree too; retain their warning,
    /// without claiming that an unrun edit has measured that many failures.
    pub fn interpolate_events(
        &self,
        other: &Self,
        events: Vec<SchedEvent>,
    ) -> Result<Self, String> {
        self.validate()?;
        other.validate()?;
        self.validate_combined_reserved_inputs(other)
            .map_err(|refusal| refusal.to_string())?;
        // The stated formats must agree, and so must the timing rules they
        // stand for here (`timing_format`): rows alone in format 6 follow
        // format 1's rules, a schedule in format 6 its own.
        if self.format_version != other.format_version
            || self.timing_format() != other.timing_format()
        {
            return Err(format!(
                "cannot interpolate record formats {:?} and {:?}: their replay rules differ; record both inputs with the same Hermit version and timing format",
                self.format_version, other.format_version
            ));
        }
        if events == self.global {
            return Ok(self.clone());
        }
        if events == other.global {
            return Ok(other.clone());
        }
        if !lists_no_time_skips(&self.time_skips) || !lists_no_time_skips(&other.time_skips) {
            return Err(
                "cannot interpolate a mixed schedule with recorded time jumps: no source's complete event order matches the result; replay an unchanged endpoint instead"
                    .to_owned(),
            );
        }
        if !self.callback_checkpoints.is_empty() || !other.callback_checkpoints.is_empty() {
            return Err(
                "cannot interpolate a mixed schedule with callback checkpoints: no mapping to the edited event order is established; replay an unchanged endpoint instead"
                    .to_owned(),
            );
        }
        let metadata = |source: &Self| {
            let mut record = source.clone();
            record.global.clear();
            record.per_thread.clear();
            // The mixed order is neither source's, so it keeps none of their
            // run's points: no readmissions, whose turns belong to an order the
            // result never ran, and no background starts, and its format goes
            // below theirs, as an edit's does.
            record.forget_run_points();
            record
        };
        let mut record = metadata(self);
        let mut other_metadata = metadata(other);
        if !matches!(
            self.guest_ids,
            Some(detcore_model::pid::GuestIds::Chosen { .. })
        ) && !matches!(
            other.guest_ids,
            Some(detcore_model::pid::GuestIds::Chosen { .. })
        ) {
            // Opaque provenance adds no admission rule to non-Chosen edits.
            // Keep the selected source's value without interpreting a future
            // descriptor or demanding that previously ignored fields agree.
            other_metadata.reserved_input = record.reserved_input.clone();
        }
        // HostObservedCount deliberately compares equal for ordinary replay
        // comparisons, so interpolation must check its stored value explicitly.
        if record != other_metadata
            || self.store_exclusive_failures() != other.store_exclusive_failures()
        {
            return Err(
                "cannot interpolate a mixed schedule with incompatible recording metadata (epoch, charging policy, RNG, timeslice or host inputs); record inputs with matching metadata or replay an unchanged endpoint"
                    .to_owned(),
            );
        }
        record.global = events;
        record.forget_stopless_marks_below_their_format();
        record.validate()?;
        Ok(record)
    }

    /// The `--max-timeslice` the run that produced this record drew under
    /// `--chaos-timeslice-draw=run`, if it drew one.
    pub fn chaos_timeslice(&self) -> Option<u64> {
        self.chaos_timeslice
    }

    /// The same record, stating the timeslice its run drew.
    pub fn with_chaos_timeslice(mut self, timeslice: Option<u64>) -> Self {
        self.chaos_timeslice = timeslice;
        self
    }

    /// The same record, listing `skips` as its jumps of global time, in format
    /// 2 at least when there are any. Analyze and bisect carry a source
    /// record's jumps into a trial schedule spliced from its prefix.
    pub fn with_time_skips(mut self, skips: Vec<TimeSkip>) -> Self {
        self.time_skips = (!skips.is_empty()).then_some(skips);
        if self.time_skips.is_some() && !self.lists_its_time_skips() {
            self.raise_format(TIME_SKIPS_FORMAT);
        }
        self
    }

    /// The same record in the newest format ([`PREEMPTION_RECORD_FORMAT_MAX`]),
    /// which a record from a run of this Hermit has, or the newest one below
    /// [`INSTRUCTION_OFFSET_ROWS_FORMAT`] when its rows hold no instruction offset
    /// ([`format_without_instruction_offsets`]), and below
    /// [`BACKGROUND_STARTS_FORMAT`] when the record does not note its run's
    /// background starts either ([`format_without_background_starts`]). A
    /// stated format this Hermit does not read (`reads_format`: an explicit 1,
    /// 0, or one newer than [`PREEMPTION_RECORD_FORMAT_MAX`]) stays as stated, so
    /// that reading the record still refuses it (`validate_format`).
    pub fn with_current_format(self) -> Self {
        if !reads_format(self.format_version) {
            return self;
        }
        let without_offsets =
            format_without_instruction_offsets(Some(PREEMPTION_RECORD_FORMAT_MAX));
        let format = if self.notes_background_starts() {
            without_offsets
        } else {
            format_without_background_starts(without_offsets)
        };
        self.with_format(format)
    }

    /// The same record in `format` (`None` for format 1), raised to
    /// [`INSTRUCTION_OFFSET_ROWS_FORMAT`] when its rows hold an instruction offset
    /// (`adopt_offsets_format`). Analyze and bisect require one shared timing
    /// format across the recordings they read (`hermit-cli`'s
    /// `time_skip_carry`). A format older than [`STOPLESS_SLICE_ENDS_FORMAT`],
    /// or a raise for offset rows that says the record notes no background
    /// starts, drops the record's `stopless` marks
    /// (`forget_stopless_marks_below_their_format`).
    pub fn with_format(mut self, format: Option<u32>) -> Self {
        self.format_version = format;
        // `format` says whether the record notes its starts; a raise for its
        // offsets says so again (`raise_format`), and a record that then says
        // it notes none drops its `stopless` marks.
        self.background_starts_unnoted = false;
        self.adopt_offsets_format();
        self.forget_stopless_marks_below_their_format();
        self
    }

    /// The record's format: `None` for format 1.
    pub fn format_version(&self) -> Option<u32> {
        self.format_version
    }

    /// The format whose timing rules the record follows (`None` for format
    /// 1): its own, except that rows alone (`rows_alone`, as
    /// `clone_preemptions_only` leaves rows that hold an instruction offset)
    /// in [`INSTRUCTION_OFFSET_ROWS_FORMAT`] follow format 1's, as the same
    /// rows without an offset state no format. The stated format of such a
    /// record only fences out a Hermit that predates the offsets; it claims no
    /// listed jumps and no switch-in charges, which index or replay a
    /// schedule trace it does not hold. A record whose schedule trace is only
    /// empty, such as a prefix of no events, keeps its format's timing, its
    /// listed jumps included. A format this Hermit does not read stays as
    /// stated, for `validate_format` to refuse. Every reader that keys timing
    /// on the format ([`Self::lists_its_time_skips`],
    /// [`Self::charges_replayed_switch_ins`]) reads this.
    pub fn timing_format(&self) -> Option<u32> {
        match self.format_version {
            Some(format)
                if self.rows_alone
                    && (INSTRUCTION_OFFSET_ROWS_FORMAT..=PREEMPTION_RECORD_FORMAT_MAX)
                        .contains(&format) =>
            {
                None
            }
            format => format,
        }
    }

    /// Whether a schedule replay of this record charges a replayed switch-in
    /// under which guest code runs (timing format 3 or later,
    /// [`SWITCH_IN_CHARGE_FORMAT`]; see [`Self::timing_format`]).
    pub fn charges_replayed_switch_ins(&self) -> bool {
        charges_replayed_switch_ins_in(self.timing_format())
    }

    /// Whether the record lists every jump of global time its run made
    /// (timing format 2 or later; see [`Self::timing_format`]), so that a
    /// replay makes exactly those.
    pub fn lists_its_time_skips(&self) -> bool {
        lists_time_skips_in(self.timing_format())
    }

    /// Whether the run this record came from registered queued sleeps at the
    /// first pass (`false` for registration when chosen, and for records that
    /// do not say).
    pub fn queued_sleeps_registered_at_first_pass(&self) -> bool {
        self.queued_sleeps_registered_at_first_pass
    }

    /// The same record, stating how the run it came from registered queued
    /// sleeps.
    pub fn with_queued_sleeps_registered_at_first_pass(mut self, first_pass: bool) -> Self {
        self.queued_sleeps_registered_at_first_pass = first_pass;
        self
    }

    /// The same record, stating the `--pipe-wake-latency` of the trials it
    /// came from. Every record analyze, minimize or bisect writes from trials
    /// carries it, so that a replay under another value is refused.
    pub fn with_pipe_wake_latency(mut self, latency: Option<u64>) -> Self {
        self.pipe_wake_latency = latency;
        self
    }

    /// The `--pipe-wake-latency` this record was produced under (`None` for
    /// off).
    pub fn pipe_wake_latency(&self) -> Option<u64> {
        self.pipe_wake_latency
    }

    /// The virtual-time epoch this record was produced under, if it was stored.
    pub fn epoch(&self) -> Option<DateTime<Utc>> {
        self.epoch
    }

    /// Whether the signal-state copy was on in the run this record came
    /// from, if the record says.
    pub fn signal_state_copy(&self) -> Option<bool> {
        self.signal_state_copy
    }

    /// The host files the recorded run's guest opened, if the record says.
    pub fn host_inputs(&self) -> Option<&HostInputSet> {
        self.host_inputs.as_ref()
    }

    /// The store-exclusives that failed in the run this record came from
    /// (zero where none were counted).
    pub fn store_exclusive_failures(&self) -> u64 {
        self.store_exclusive_failures.0
    }

    /// A retained source event count where a replay omitted a change point.
    /// After transformations it need not index this record's global events.
    pub fn replay_departed_at(&self) -> Option<u64> {
        self.replay_departed_at
    }

    /// Refuse a known omission before replay or analysis can certify it.
    /// Unmarked legacy records retain their existing admission policy; their
    /// absence of this field is not proof of historical completeness.
    pub fn require_faithful_replay(&self) -> anyhow::Result<()> {
        if let Some(event) = self.replay_departed_at {
            anyhow::bail!(
                "HERMIT_INCOMPLETE_REPLAY_RECORD: replay_departed_at={event}: this record omits \
                 a replay change point and cannot certify faithful replay. Re-record a complete \
                 execution; --replay-edited-preemptions permits only non-verifying history exploration."
            );
        }
        Ok(())
    }

    /// Retain an omission observed in a source record, never clearing one.
    /// When sources differ, the smaller count is only a retained source
    /// observation: it is neither a common causal order nor an event index
    /// in a transformed output. Admission depends only on its presence.
    pub fn retain_replay_departure(mut self, source: Option<u64>) -> Self {
        self.replay_departed_at = match (self.replay_departed_at, source) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self
    }

    /// Whether the run this record came from rewrote exclusive loops in the
    /// guest's memory.
    pub fn rewrite_exclusives_in_memory(&self) -> bool {
        self.rewrite_exclusives_in_memory
    }

    /// The same record, stating whether the trials it came from rewrote
    /// exclusive loops in the guest's memory; analyze stamps it on every
    /// record it writes, as it stamps the pipe-wake latency. Stating it on
    /// states the current rewriter's provenance with it
    /// (`with_rewrite_exclusives`); stating it off clears any provenance,
    /// so use that setter for an overlay rewrite.
    pub fn with_rewrite_exclusives_in_memory(self, on: bool) -> Self {
        self.with_rewrite_exclusives(
            on.then(|| RewriteExclusives::current(RewriteExclusivesMode::Memory)),
        )
    }

    /// The exclusive-loop rewrite the run this record came from applied,
    /// if it states one.
    pub fn rewrite_exclusives(&self) -> Option<RewriteExclusives> {
        self.rewrite_exclusives
    }

    /// The same record, stating the exclusive-loop rewrite the trials it
    /// came from applied, if any; analyze stamps it on every record it
    /// writes. The older in-memory field is set to agree.
    pub fn with_rewrite_exclusives(mut self, rewrite: Option<RewriteExclusives>) -> Self {
        self.rewrite_exclusives = rewrite;
        self.rewrite_exclusives_in_memory = rewrite.is_some_and(|rewrite| rewrite.in_memory());
        self
    }

    /// The same record, stating whether the copy was on in the trials it
    /// came from (`None` for unknown). Every record analyze, minimize or
    /// bisect writes from trials carries it, as it carries the pipe-wake
    /// latency.
    pub fn with_signal_state_copy(mut self, on: Option<bool>) -> Self {
        self.signal_state_copy = on;
        self
    }

    /// The same record, stating whether the trials it came from capped
    /// preemption charges, as it states the pipe-wake latency.
    pub fn with_cap_preemption_charge(mut self, on: bool) -> Self {
        self.cap_preemption_charge = on;
        self
    }

    /// Whether this record was produced under `--cap-preemption-charge`.
    pub fn cap_preemption_charge(&self) -> bool {
        self.cap_preemption_charge
    }

    /// How the run this record came from numbered its guest, if no PID
    /// namespace of its own did.
    pub fn guest_ids(&self) -> Option<detcore_model::pid::GuestIds> {
        self.guest_ids
    }

    /// The same record, retaining the guest numbering of schedules used to
    /// construct it. Namespace numbering keeps the legacy omitted field.
    pub fn with_guest_ids(mut self, ids: detcore_model::pid::GuestIds) -> Self {
        self.guest_ids = (ids != detcore_model::pid::GuestIds::Namespace).then_some(ids);
        self
    }

    /// The post-crash bound in effect in the run this record came from
    /// (`None` for off).
    pub fn post_crash_timeslice(&self) -> Option<u64> {
        self.post_crash_timeslice
    }

    /// Where the run this record came from took the post-crash switch, each
    /// time it did.
    pub fn post_crash(&self) -> &[crate::post_crash::PostCrashSwitch] {
        self.post_crash.as_deref().unwrap_or(&[])
    }

    /// The switches, if this record is one its own run wrote with the switch
    /// on (and so lists every switch it took, possibly none).
    pub fn post_crash_complete(&self) -> Option<&[crate::post_crash::PostCrashSwitch]> {
        self.post_crash.as_deref()
    }

    /// The same record, stating the post-crash bound of the trials it came
    /// from (`None` for off). Every record analyze, minimize or bisect writes
    /// from trials carries it, as it carries the pipe-wake latency.
    pub fn with_post_crash_timeslice(mut self, bound: Option<u64>) -> Self {
        self.post_crash_timeslice = bound;
        self
    }

    /// Make a copy of everything in the `PreemptionRecord` in a public format.
    pub fn extract_all(&self) -> BTreeMap<DetTid, ThreadHistory> {
        self.per_thread.clone()
    }

    /// Inverse of from_vecs.
    pub fn as_vecs(&self) -> BTreeMap<DetTid, Vec<(LogicalTime, Priority)>> {
        let mut bt = BTreeMap::new();
        for (tid, th) in &self.per_thread {
            bt.insert(*tid, th.as_vec());
        }
        bt
    }
    /// Drop what names points of the recorded run itself, for an edited
    /// record whose schedule is no longer that run's: the callback
    /// checkpoints and the readmissions of backgrounded calls. A replay of the
    /// edited record then readmits a finished call when it finds it finished,
    /// as before readmissions were recorded, rather than at points of a
    /// schedule it no longer follows.
    fn forget_run_points(&mut self) {
        self.callback_checkpoints.clear();
        self.rejoins = None;
        // The background starts sit between events an edit moves, so they go
        // too: the edited record notes no start, and keeps the timing rules
        // of its format.
        self.forget_background_starts();
    }

    /// Drop the background starts, for a record that no longer holds every
    /// start its run took, and say that it notes none. Its format goes below
    /// [`BACKGROUND_STARTS_FORMAT`] ([`format_without_background_starts`]),
    /// with the same timing rules, unless its rows hold an instruction
    /// offset: then it keeps [`INSTRUCTION_OFFSET_ROWS_FORMAT`], which a
    /// Hermit that predates the offsets must go on refusing, and says it
    /// notes no starts by its mark (`background_starts_unnoted`, set by
    /// `raise_format`), as [`PREEMPTION_RECORD_FORMAT_MAX`] describes. Either way
    /// it drops the `stopless` marks its ends stay visible without, which
    /// only a record that notes its starts holds; a mark whose end would
    /// become invisible is kept for validation to refuse
    /// (`forget_stopless_marks_below_their_format`).
    fn forget_background_starts(&mut self) {
        self.background_starts.clear();
        self.background_starts_unnoted = false;
        self.format_version = format_without_background_starts(self.format_version);
        self.adopt_offsets_format();
        self.forget_stopless_marks_below_their_format();
    }

    /// TODO: provide a better way to create an empty PreemptionRecord but with entries for a given set of tids.
    pub fn strip_contents(mut self) -> Self {
        self.forget_run_points();
        for history in self.per_thread.values_mut() {
            history.prio_changes = Vec::new();
            history.preemption_rcbs = Vec::new();
            history.preemption_instruction_offsets = Vec::new();
            history.chaos_epochs = Vec::new();
            history.final_prio = 1000;
        }
        self.global = Vec::new();
        self
    }

    /// Iterates over each sched event and applies function event -> *event yielding resulting iterator as new events
    /// This effectively moves content of global allowing to split or delete events based on the function provided
    pub fn split_map<F, R>(&mut self, splitter: F)
    where
        R: IntoIterator<Item = SchedEvent>,
        F: Fn(SchedEvent, &ReplayCursor<SchedEvent>) -> R,
    {
        self.forget_run_points();
        let mut result = Vec::new();
        let global = std::mem::take(&mut self.global);
        let mut cursor = ReplayCursor::from_iter(global);

        while let Some(event) = cursor.next() {
            for new_event in splitter(event, &cursor) {
                result.push(new_event);
            }
        }
        self.global = result;
    }

    /// Iterate over the schedevents.
    pub fn schedevents_iter_mut(&mut self) -> std::slice::IterMut<'_, SchedEvent> {
        self.forget_run_points();
        self.global.iter_mut()
    }

    /// Leave only the per-thread preemption records, not the global schedule.
    /// Rows that hold an instruction offset are then rows alone, as
    /// [`Self::clone_preemptions_only`] leaves them: they say so
    /// (`rows_alone`), state [`INSTRUCTION_OFFSET_ROWS_FORMAT`] and keep no
    /// jumps of global time, which index the schedule trace they no longer
    /// hold. A record of a format this Hermit does not read keeps its format
    /// as stated, for `validate_format` to refuse.
    pub fn preemptions_only(&mut self) {
        self.forget_run_points();
        self.global.clear();
        if self.rows_carry_instruction_offsets() && reads_format(self.format_version) {
            self.format_version = Some(INSTRUCTION_OFFSET_ROWS_FORMAT);
            self.rows_alone = true;
            // Rows alone note no starts by that mark, as a copy's do
            // (`clone_preemptions_only`).
            self.background_starts_unnoted = false;
            self.time_skips = None;
        }
    }

    /// Make a copy just of the preemptions (cheap) rather than the whole schedule trace (expensive)
    pub fn clone_preemptions_only(&self) -> Self {
        PreemptionRecord {
            per_thread: self.per_thread.clone(),
            global: Vec::new(),
            epoch: self.epoch,
            pipe_wake_latency: self.pipe_wake_latency,
            signal_state_copy: self.signal_state_copy,
            cap_preemption_charge: self.cap_preemption_charge,
            chaos_timeslice: self.chaos_timeslice,
            guest_ids: self.guest_ids,
            // A record derived from an inexact one is inexact too.
            replay_departed_at: self.replay_departed_at,
            reserved_input: self.reserved_input.clone(),
            post_crash_timeslice: self.post_crash_timeslice,
            // A derived record (minimize, analyze trials) runs a different
            // schedule, whose switches the run computes afresh.
            post_crash: None,
            callback_checkpoints: BTreeMap::new(),
            rng_seed: self.rng_seed,
            expired_wait_runs_charged_once: self.expired_wait_runs_charged_once,
            host_inputs: self.host_inputs.clone(),
            // A count of this run's host, not part of the preemptions.
            store_exclusive_failures: HostObservedCount::default(),
            // Rows alone claim no timing format. Rows that hold an instruction
            // offset still state the format a Hermit before the offsets
            // refuses (`INSTRUCTION_OFFSET_ROWS_FORMAT`), whose rows-only timing is
            // format 1's (`timing_format`).
            format_version: self
                .rows_carry_instruction_offsets()
                .then_some(INSTRUCTION_OFFSET_ROWS_FORMAT),
            time_skips: None,
            queued_sleeps_registered_at_first_pass: self.queued_sleeps_registered_at_first_pass,
            rejoins: None,
            background_starts: Vec::new(),
            rewrite_exclusives_in_memory: self.rewrite_exclusives_in_memory,
            rows_alone: self.rows_carry_instruction_offsets(),
            // Rows alone note no starts by that mark (`notes_background_starts`).
            background_starts_unnoted: false,
            rewrite_exclusives: self.rewrite_exclusives,
        }
    }

    /// Read-only access to the raw event schedule.
    pub fn schedevents(&self) -> &Vec<SchedEvent> {
        &self.global
    }

    /// Returns true if the schedule trace (SchedEvent) record is nonempty.
    pub fn contains_schedevents(&self) -> bool {
        !self.global.is_empty()
    }

    /// Convert from a flat vector representation (Time,Priority_AFTER_Time) into the internal representation.
    /// The very first timeslice should have a zero timestamp, but it is ignored if nonzero.
    pub fn from_vecs(bt: &BTreeMap<DetTid, Vec<(LogicalTime, Priority)>>) -> Self {
        let mut bt2 = BTreeMap::new();
        for (tid, vec) in bt {
            let th = if vec.is_empty() {
                ThreadHistory {
                    final_prio: DEFAULT_PRIORITY,
                    prio_changes: Vec::new(),
                    preemption_rcbs: Vec::new(),
                    preemption_instruction_offsets: Vec::new(),
                    chaos_epochs: Vec::new(),
                }
            } else {
                // We could insist on this invariant, but it's a little more flexible not to:
                // assert!(vec.get(0).unwrap().per_thread.is_zero());
                let (_final_end, final_prio) = vec.last().unwrap();
                let final_prio = *final_prio;

                let mut prio_changes = Vec::new();
                let mut it = vec.iter().peekable();
                while let Some((_this_ns, this_p)) = it.next() {
                    if let Some((next_ns, _next_p)) = it.peek() {
                        prio_changes.push((*next_ns, *this_p));
                    } else {
                        // Don't need the last one, because we've already used it's
                        // timestamp, and retrieved final_prio.
                        break;
                    }
                }
                assert!(final_prio >= FIRST_PRIORITY);
                assert!(final_prio <= LAST_PRIORITY);
                ThreadHistory {
                    final_prio,
                    prio_changes,
                    preemption_rcbs: Vec::new(),
                    preemption_instruction_offsets: Vec::new(),
                    chaos_epochs: Vec::new(),
                }
            };
            bt2.insert(*tid, th);
        }
        PreemptionRecord {
            per_thread: bt2,
            global: Vec::new(),
            epoch: None,
            pipe_wake_latency: None,
            signal_state_copy: None,
            cap_preemption_charge: false,
            chaos_timeslice: None,
            guest_ids: None,
            replay_departed_at: None,
            reserved_input: ReservedInputMetadata::default(),
            post_crash_timeslice: None,
            post_crash: None,
            callback_checkpoints: BTreeMap::new(),
            rng_seed: None,
            expired_wait_runs_charged_once: false,
            host_inputs: None,
            store_exclusive_failures: HostObservedCount::default(),
            format_version: None,
            time_skips: None,
            queued_sleeps_registered_at_first_pass: false,
            rejoins: None,
            background_starts: Vec::new(),
            rewrite_exclusives_in_memory: false,
            rows_alone: false,
            background_starts_unnoted: false,
            rewrite_exclusives: None,
        }
    }

    /// The jumps of global time the recorder saw (`time_skips`), or `None`
    /// when the record does not say.
    pub fn time_skips(&self) -> Option<&[TimeSkip]> {
        self.time_skips.as_deref()
    }

    /// The background starts the record holds, in order (`BackgroundStart`).
    pub fn background_starts(&self) -> &[BackgroundStart] {
        &self.background_starts
    }

    /// Whether the record notes every background start its run took: a
    /// record of `BACKGROUND_STARTS_FORMAT` or newer, unless it says it notes
    /// none (`background_starts_unnoted`) or holds rows alone (`rows_alone`),
    /// without the schedule a start sits in. A format-1 record says nothing
    /// about when its calls began.
    pub fn notes_background_starts(&self) -> bool {
        self.format_version
            .is_some_and(|format| format >= BACKGROUND_STARTS_FORMAT)
            && !self.background_starts_unnoted
            && !self.rows_alone
    }

    pub(crate) fn callback_checkpoints(
        &self,
    ) -> &BTreeMap<DetTid, Vec<callback_history::Checkpoint>> {
        &self.callback_checkpoints
    }

    /// Whether any thread's rows carry an instruction offset
    /// (`ThreadHistory::preemption_instruction_offsets`).
    pub fn rows_carry_instruction_offsets(&self) -> bool {
        self.per_thread
            .values()
            .any(ThreadHistory::has_instruction_offsets)
    }

    /// State [`INSTRUCTION_OFFSET_ROWS_FORMAT`] when the rows carry an instruction
    /// offset: a Hermit that predates the offsets must refuse the record
    /// rather than replay its rows without them.
    fn adopt_offsets_format(&mut self) {
        if self.rows_carry_instruction_offsets() {
            self.raise_format(INSTRUCTION_OFFSET_ROWS_FORMAT);
        }
    }

    /// State at least `format`: a record holds every feature up to the
    /// format it states, so a feature only ever raises it, and never lowers
    /// what another feature stated. A stated format this Hermit does not
    /// read ([`reads_format`]: 0, an explicit 1, or one above
    /// [`PREEMPTION_RECORD_FORMAT_MAX`]) stays as stated, so that reading the
    /// record still refuses it (`validate_format`): raised, it would make a
    /// refused record readable.
    ///
    /// A record that noted no background starts and is raised to a format at
    /// or above [`BACKGROUND_STARTS_FORMAT`] still notes none, and says so
    /// (`background_starts_unnoted`): the number alone would claim them.
    fn raise_format(&mut self, format: u32) {
        let noted = self.notes_background_starts();
        self.format_version = Some(match self.format_version {
            None => format,
            Some(stated) if !reads_format(Some(stated)) => stated,
            Some(stated) => stated.max(format),
        });
        if !noted
            && !self.rows_alone
            && reads_format(self.format_version)
            && self
                .format_version
                .is_some_and(|stated| stated >= BACKGROUND_STARTS_FORMAT)
        {
            self.background_starts_unnoted = true;
        }
    }

    /// Timestamp stripping preserves the event/progress relation, unlike an editor.
    pub(crate) fn strip_event_times(&mut self) {
        for event in &mut self.global {
            event.end_time = None;
        }
    }

    /// Extracts the inner global events.
    pub fn into_global(self) -> Vec<SchedEvent> {
        self.global
    }

    /// For each recorded callback checkpoint, by thread and number
    /// (`history_index`): the index in the global schedule of the thread's
    /// event that ends its slice, the event whose running count of the
    /// thread's branches and instructions reaches the checkpoint's progress,
    /// as `validate_callback_checkpoints` finds it. A checkpoint at progress
    /// 0, before the thread's first event, has none
    /// (<https://github.com/rrnewton/hermit/issues/4876>).
    pub(crate) fn callback_checkpoint_events(&self) -> BTreeMap<(DetTid, usize), u64> {
        let mut endpoints: BTreeMap<DetTid, BTreeMap<u64, u64>> = BTreeMap::new();
        let mut progress = BTreeMap::<DetTid, u64>::new();
        for (index, event) in self.global.iter().enumerate() {
            if event.count == 0 || !self.callback_checkpoints.contains_key(&event.dettid) {
                continue;
            }
            let count = progress.entry(event.dettid).or_default();
            *count = count.saturating_add(u64::from(event.count));
            endpoints
                .entry(event.dettid)
                .or_default()
                .insert(*count, index as u64);
        }
        let mut events = BTreeMap::new();
        for (tid, checkpoints) in &self.callback_checkpoints {
            for checkpoint in checkpoints {
                if let Some(index) = endpoints
                    .get(tid)
                    .and_then(|points| points.get(&checkpoint.origin.progress))
                {
                    events.insert((*tid, checkpoint.history_index), *index);
                }
            }
        }
        events
    }

    /// Save to disk.
    pub fn write_to_disk(&self, path: &Path) -> Result<(), String> {
        streaming::write_file(path, self).map_err(|error| {
            format!(
                "Failed to write preemption record to file {:?}, error: {error:#}",
                path
            )
        })
    }

    /// Perform internal invariant checks on the PreemptionRecord and return an error if
    /// it is not well formed.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_contents()?;
        self.validate_format()
    }

    /// Check everything [`Self::validate`] does but the format
    /// ([`Self::validate_format`]), whose failure is a refusal rather than a
    /// malformed record, and which readers check separately for that reason.
    pub fn validate_contents(&self) -> Result<(), String> {
        let events = self.global.len() as u64;
        if let Some(start) = self
            .background_starts
            .iter()
            .zip(self.background_starts.iter().skip(1))
            .find(|(a, b)| a.after > b.after)
            .map(|(_, b)| b)
            .or_else(|| {
                self.background_starts
                    .iter()
                    .find(|start| start.after > events)
            })
        {
            return Err(format!(
                "background start of call {} of thread {} after event {} is out of order or \
                 past the {} recorded events",
                start.call, start.thread, start.after, events
            ));
        }
        // The mark is judged in a format this Hermit reads; any other is
        // refused by name, whatever the record holds (`validate_format`).
        if self.background_starts_unnoted && reads_format(self.format_version) {
            if !self.background_starts.is_empty() {
                return Err(format!(
                    "the preemption record says it notes no background starts \
                     (background_starts_unnoted) but holds {}",
                    self.background_starts.len()
                ));
            }
            if self
                .format_version
                .is_none_or(|format| format < INSTRUCTION_OFFSET_ROWS_FORMAT)
            {
                return Err(format!(
                    "the preemption record says it notes no background starts \
                     (background_starts_unnoted) in format {}, below the format \
                     {INSTRUCTION_OFFSET_ROWS_FORMAT}, the first that keeps its number without \
                     them",
                    self.format_version.unwrap_or(1)
                ));
            }
            let stopless = self.global.iter().filter(|event| event.stopless).count();
            if stopless > 0 {
                return Err(format!(
                    "the preemption record says it notes no background starts \
                     (background_starts_unnoted) but marks {stopless} timeslice ends taken \
                     without a stop (`stopless`), which only a record that notes its starts holds"
                ));
            }
        }
        if !self.background_starts.is_empty() && !self.notes_background_starts() {
            return Err(format!(
                "the preemption record holds background starts but is format {}, older than \
                 the format {BACKGROUND_STARTS_FORMAT} that notes them",
                self.format_version.unwrap_or(1)
            ));
        }
        // A record's readmissions are kept as stored, whatever its format
        // (main's schedule replays re-record them in format 3), and every
        // loader refuses the same ones: those a replay could not walk once,
        // in order (`read_recorded_rejoins` reads only this field, by the
        // same rule).
        if let Some(rejoins) = &self.rejoins {
            crate::scheduler::rejoin_log::ordered(rejoins)?;
        }
        if self.rows_alone && !self.global.is_empty() {
            return Err("a record of rows alone (rows_alone) holds a schedule trace".to_string());
        }
        self.validate_callback_checkpoints()?;
        self.validate_histories()
    }

    /// Check the record's format and its jumps of global time: a format this
    /// Hermit reads, with the list that format requires, in stream order, to
    /// later and finite deadlines. A failure is a refusal of the record, not a
    /// malformed file (`RecordFormatRefusal`).
    pub fn validate_format(&self) -> Result<(), String> {
        if let Some(skip) = self
            .time_skips
            .iter()
            .flatten()
            .find(|skip| skip.target.is_indefinite())
        {
            return Err(format!(
                "time skip {skip:?} moves to the indefinite deadline, which nothing reaches"
            ));
        }
        let skips = match &self.time_skips {
            Some(skips) if skips.is_empty() => {
                return Err("an empty list of time skips is written as no list".to_string());
            }
            skips => skips.as_deref().unwrap_or_default(),
        };
        if reads_format(self.format_version)
            && self.rows_carry_instruction_offsets()
            && self
                .format_version
                .is_none_or(|format| format < INSTRUCTION_OFFSET_ROWS_FORMAT)
        {
            return Err(format!(
                "record format {:?} with instruction offsets in its rows: offsets need format \
                 {INSTRUCTION_OFFSET_ROWS_FORMAT}, which a Hermit that cannot replay them refuses",
                self.format_version
            ));
        }
        let known = reads_format(self.format_version);
        if let Some((index, refusal)) = self
            .global
            .iter()
            .enumerate()
            .find_map(|(index, event)| Some((index, event.stopless_mark_refusal()?)))
        {
            return Err(format!("record event {index}: {refusal}"));
        }
        let stopless = self.global.iter().filter(|event| event.stopless).count();
        if stopless > 0
            && self
                .format_version
                .is_none_or(|format| format < STOPLESS_SLICE_ENDS_FORMAT)
        {
            return Err(format!(
                "record format {:?} with {stopless} timeslice ends taken without a stop \
                 (`stopless`): such ends need format {STOPLESS_SLICE_ENDS_FORMAT} or later",
                self.format_version
            ));
        }
        if !known || (!skips.is_empty() && !self.lists_its_time_skips()) {
            return Err(format!(
                "record format {:?} with {} time skips: this Hermit reads formats 1 to \
                 {PREEMPTION_RECORD_FORMAT_MAX}, and time skips need format {TIME_SKIPS_FORMAT} or later",
                self.format_version,
                skips.len()
            ));
        }
        if let Some(pair) = skips
            .windows(2)
            .find(|pair| pair[1].event < pair[0].event || pair[1].target <= pair[0].target)
        {
            return Err(format!(
                "time skips must come in stream order and move to later deadlines: {:?} then {:?}",
                pair[0], pair[1]
            ));
        }
        Ok(())
    }

    /// Check every thread's priority history.
    fn validate_histories(&self) -> Result<(), String> {
        for (tid, history) in &self.per_thread {
            if !is_ordinary_priority(history.final_prio) {
                return Err(format!(
                    "final priority for thread {} invalid: {}",
                    tid, history.final_prio
                ));
            }
            {
                let mut time_last = None;
                for (count, (ns, prio)) in history.prio_changes.iter().enumerate() {
                    if let Some(last) = time_last {
                        if !is_ordinary_priority(*prio) {
                            return Err(format!(
                                "preemption priority #{} for thread {} invalid: {}",
                                count, tid, prio
                            ));
                        }
                        if *ns <= last {
                            return Err(format!(
                                "thread {}'s change point {} at {} is not after change point {} \
                                 at {} (counting from 1): a thread's change points must be \
                                 strictly increasing in time",
                                tid,
                                count + 1,
                                ns,
                                count,
                                last
                            ));
                        }
                    }
                    time_last = Some(*ns);
                }
            }
            if !history.preemption_rcbs.is_empty()
                && history.preemption_rcbs.len() != history.prio_changes.len()
            {
                return Err(format!(
                    "thread {} has {} preemption times but {} RCB targets",
                    tid,
                    history.prio_changes.len(),
                    history.preemption_rcbs.len()
                ));
            }
            if !history.preemption_instruction_offsets.is_empty()
                && history.preemption_instruction_offsets.len() != history.prio_changes.len()
            {
                return Err(format!(
                    "thread {} has {} preemption times but {} instruction offsets",
                    tid,
                    history.prio_changes.len(),
                    history.preemption_instruction_offsets.len()
                ));
            }
            if !history.preemption_instruction_offsets.is_empty()
                && history.preemption_rcbs.is_empty()
            {
                return Err(format!(
                    "thread {} has instruction offsets but no RCB targets",
                    tid
                ));
            }
            if history
                .preemption_instruction_offsets
                .iter()
                .flatten()
                .any(|offset| offset.instructions == 0)
            {
                return Err(format!(
                    "thread {} has an instruction offset of zero steps",
                    tid
                ));
            }
            // A row records an offset only once its slice reached the target
            // (`ThreadState::take_ended_slice_offset`). There is no upper
            // bound: an `rt_sigreturn` among the steps cancels the event, and
            // the slice then ends at its next callback.
            if history
                .preemption_instruction_offsets
                .iter()
                .zip(&history.preemption_rcbs)
                .any(|(offset, rcbs)| offset.is_some_and(|offset| *rcbs < offset.rcb_target))
            {
                return Err(format!(
                    "thread {} has an instruction offset whose branch target its row's end precedes",
                    tid
                ));
            }
            if history
                .preemption_rcbs
                .windows(2)
                .any(|pair| pair[1] < pair[0])
            {
                return Err(format!(
                    "preemption RCB targets failed to increase for thread {}",
                    tid
                ));
            }
            let mut transition_last = None;
            let mut epoch_last = None;
            for transition in &history.chaos_epochs {
                if transition.factor.as_f64() <= 0.0 {
                    return Err(format!(
                        "chaos epoch factor for thread {} must be positive",
                        tid
                    ));
                }
                if transition_last.is_some_and(|last| transition.logical_time <= last) {
                    return Err(format!(
                        "chaos epoch transition times failed to increase for thread {}",
                        tid
                    ));
                }
                if epoch_last.is_some_and(|last| transition.epoch <= last) {
                    return Err(format!(
                        "chaos epoch numbers failed to increase for thread {}",
                        tid
                    ));
                }
                transition_last = Some(transition.logical_time);
                epoch_last = Some(transition.epoch);
            }
        }
        Ok(())
    }

    fn validate_callback_checkpoints(&self) -> Result<(), String> {
        let mut endpoints: BTreeMap<DetTid, BTreeMap<u64, crate::types::Op>> = BTreeMap::new();
        let mut progress = BTreeMap::<DetTid, u64>::new();
        for event in self.global.iter().filter(|event| {
            event.count > 0 && self.callback_checkpoints.contains_key(&event.dettid)
        }) {
            let count = progress.entry(event.dettid).or_default();
            *count = count
                .checked_add(u64::from(event.count))
                .ok_or("callback progress overflow")?;
            endpoints
                .entry(event.dettid)
                .or_default()
                .insert(*count, event.op);
        }
        for (tid, checkpoints) in &self.callback_checkpoints {
            let history = self
                .per_thread
                .get(tid)
                .ok_or("callback history has no thread")?;
            let mut previous = None;
            for checkpoint in checkpoints {
                let row = checkpoint.history_index;
                if previous.is_some_and(|(last_row, last_progress)| {
                    row <= last_row || checkpoint.origin.progress < last_progress
                }) || history.prio_changes.get(row).map(|x| x.0) != Some(checkpoint.origin.time)
                    || history.preemption_rcbs.get(row).copied() != Some(checkpoint.origin.rcbs)
                {
                    return Err(format!("invalid callback history row for {tid}: {row}"));
                }
                previous = Some((row, checkpoint.origin.progress));
                let found = if checkpoint.origin.progress == 0 {
                    checkpoint.origin.last_op.is_none()
                } else {
                    endpoints
                        .get(tid)
                        .and_then(|points| points.get(&checkpoint.origin.progress))
                        == checkpoint.origin.last_op.as_ref()
                        && checkpoint.origin.last_op.is_some()
                };
                if !found {
                    return Err(format!(
                        "callback history has no event endpoint for {tid}: {row}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Normalize priorities for readability: renumber them from
    /// `DEFAULT_PRIORITY` up, and drop the threads that keep the default
    /// priority. For display only: the renumbered record does not replay the
    /// same, because the scheduler compares these priorities with fixed ones
    /// (the default priority of a thread the record does not name, among
    /// others) and derives others from them. A record with more distinct
    /// priorities than `DEFAULT_PRIORITY..=LAST_PRIORITY` holds (a long chaos
    /// run) keeps its own.
    pub fn normalize(&self) -> PreemptionRecord {
        let mut clone = self.clone();

        let mut priomap: BTreeMap<Priority, Priority> = BTreeMap::new();
        for history in clone.per_thread.values_mut() {
            let _ = priomap.insert(history.final_prio, 0);
            for (_ns, prio) in &history.prio_changes {
                let _ = priomap.insert(*prio, 0);
            }
        }
        if priomap.len() > usize::try_from(LAST_PRIORITY - DEFAULT_PRIORITY + 1).unwrap() {
            return clone.without_default_threads();
        }

        for (cur_prio, val) in (DEFAULT_PRIORITY..).zip(priomap.values_mut()) {
            *val = cur_prio;
        }
        for history in clone.per_thread.values_mut() {
            history.final_prio = *priomap.get(&history.final_prio).unwrap();
            for (_ns, prio) in &mut history.prio_changes {
                *prio = *priomap.get(prio).unwrap();
            }
        }

        // One more pass to remove anything that just has default priority its whole lifetime.
        clone.without_default_threads()
    }

    /// The same record without the threads that keep the default priority
    /// their whole lifetime, with no priority change or chaos epoch. Replay
    /// gives a thread the record does not name that same priority, so the
    /// record replays the same (Detcore warns once per such thread); unlike
    /// [`Self::normalize`], no priority is renumbered.
    pub fn without_default_threads(&self) -> PreemptionRecord {
        let mut clone = self.clone();
        clone.per_thread.retain(|_, history| {
            history.final_prio != DEFAULT_PRIORITY
                || !history.prio_changes.is_empty()
                || !history.chaos_epochs.is_empty()
        });
        clone
    }

    /// Remove whichever is the latest preemption across all thread histories. If
    /// there are no priority changes in any thread history, the entire last thread
    /// (highest thread number) is completely removed.
    /// Note this function assumes the last priority change in the list has the latest
    /// preempt time.
    pub fn with_latest_preempt_removed(&self) -> PreemptionRecord {
        let mut preempts_latest_prio_changes: Vec<(DetTid, LogicalTime)> = self
            .per_thread
            .clone()
            .into_iter()
            .map(|(tid, th)| {
                (
                    tid,
                    th.prio_changes
                        .last() // Assume last prio change in the list has the latest preempt time
                        .map_or(LogicalTime::ZERO, |prio_change| prio_change.0),
                )
            })
            .collect();
        preempts_latest_prio_changes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        let tid_with_preempt_to_drop = preempts_latest_prio_changes
            .into_iter()
            .map(|(tid, _th)| tid)
            .next();

        let mut clone = self.clone();
        clone.forget_run_points();
        if let Some(tid) = tid_with_preempt_to_drop {
            let thread_history = &mut clone.per_thread.get_mut(&tid).unwrap();
            if !thread_history.prio_changes.is_empty() {
                // There were prio changes for at least one of the threads, remove that
                // preemption as the one to drop
                let removed_prio = thread_history.prio_changes.pop().unwrap().1;
                if !thread_history.preemption_rcbs.is_empty() {
                    thread_history.preemption_rcbs.pop();
                }
                if !thread_history.preemption_instruction_offsets.is_empty() {
                    thread_history.preemption_instruction_offsets.pop();
                }
                thread_history.final_prio = removed_prio;
            } else {
                // There were no prio changes for any threads, so just remove the last
                // thread completely as the drop
                clone.per_thread.pop_last();
            }
        }
        clone
    }
}

/// The record of priorities and preemptions for a particular thread.
#[derive(
    PartialEq, // Silly protection from rustfmt disagreements.
    Debug,
    Eq,
    Clone,
    Hash,
    Deserialize,
)]
pub struct ThreadHistory {
    /// The priority of the last timeslice, after the last preemption.
    /// (This is also the initial priority if there are no preemptions.)
    /// Plain nochaos/default-heuristic raw replay retains this logical policy
    /// value; its temporary foreground/deferred dispatch bands are not changes
    /// to the recorded policy.
    pub final_prio: Priority,

    /// The preemption points and priority just BEFORE the preemption occurred.  It's
    /// private, and thus you must go through the iterator, as this will change in the
    /// future.
    prio_changes: Vec<(LogicalTime, Priority)>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Exact per-thread PMU RCB target for each matching priority change. Empty
    /// in older JSON artifacts, which fall back to logical-time replay.
    // `ThreadHistory` crosses a non-self-describing bincode RPC; never skip it.
    #[serde(default)]
    preemption_rcbs: Vec<u64>,

    /// Where each matching timeslice's timer was armed, for a timeslice that
    /// `--chaos-instruction-offset` delivered past its branch target (`None`
    /// for the others). Empty when no timeslice had one, as in every record
    /// written without the option and by older versions; a written record
    /// then omits it (see `Serialize`).
    #[serde(default)]
    preemption_instruction_offsets: Vec<Option<InstructionOffset>>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Exact per-thread slowdown transitions. Empty artifacts from older Hermit
    /// versions remain wire-compatible when read through Serde's default.
    // `ThreadHistory` also crosses Reverie's non-self-describing bincode RPC,
    // so this field must never be skipped when it is empty.
    #[serde(default)]
    chaos_epochs: Vec<ChaosEpochTransition>,
}

/// Written by hand only to leave out empty instruction offsets from a written
/// (human-readable) record, so that a record made without
/// `--chaos-instruction-offset` is byte for byte what it was before the
/// option existed. `ThreadHistory` also crosses Detcore's RPC, whose encoding
/// is not self-describing and needs every field, in declaration order.
impl Serialize for ThreadHistory {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let omit_offsets =
            serializer.is_human_readable() && self.preemption_instruction_offsets.is_empty();
        let mut fields =
            serializer.serialize_struct("ThreadHistory", if omit_offsets { 4 } else { 5 })?;
        fields.serialize_field("final_prio", &self.final_prio)?;
        fields.serialize_field("prio_changes", &self.prio_changes)?;
        fields.serialize_field("preemption_rcbs", &self.preemption_rcbs)?;
        if omit_offsets {
            fields.skip_field("preemption_instruction_offsets")?;
        } else {
            fields.serialize_field(
                "preemption_instruction_offsets",
                &self.preemption_instruction_offsets,
            )?;
        }
        fields.serialize_field("chaos_epochs", &self.chaos_epochs)?;
        fields.end()
    }
}

impl ThreadHistory {
    /// An empty thread history, with default priority only.
    pub fn new() -> Self {
        ThreadHistory {
            final_prio: DEFAULT_PRIORITY,
            prio_changes: Vec::new(),
            preemption_rcbs: Vec::new(),
            preemption_instruction_offsets: Vec::new(),
            chaos_epochs: Vec::new(),
        }
    }

    /// Turns the thread history into an iterator that produces a stream of preemption events.
    #[allow(clippy::should_implement_trait)]
    pub fn into_iter(self) -> ThreadHistoryIterator {
        ThreadHistoryIterator {
            full_history: self,
            ix: 0,
            chaos_epoch_ix: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_chaos_epochs(mut self, chaos_epochs: Vec<ChaosEpochTransition>) -> Self {
        self.chaos_epochs = chaos_epochs;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_prio_changes(mut self, prio_changes: Vec<(LogicalTime, Priority)>) -> Self {
        self.prio_changes = prio_changes;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_preemption_rcbs(mut self, preemption_rcbs: Vec<u64>) -> Self {
        self.preemption_rcbs = preemption_rcbs;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_preemption_instruction_offsets(
        mut self,
        offsets: Vec<Option<InstructionOffset>>,
    ) -> Self {
        self.preemption_instruction_offsets = offsets;
        self
    }

    /// Whether any timeslice of this history was delivered past its branch
    /// target (`--chaos-instruction-offset`).
    pub fn has_instruction_offsets(&self) -> bool {
        self.preemption_instruction_offsets
            .iter()
            .any(Option::is_some)
    }

    /// Flatten the time series into a uniform representation.
    /// This is a `(Time, Priority_AFTER_Time)` representation.
    /// The very first timeslice will have a zero timestamp.
    // Note that this is different than the internal representation which groups together
    // `Priority_UNTIL_Time, Time`, i.e. shifted by one.
    pub fn as_vec(&self) -> Vec<(LogicalTime, Priority)> {
        let mut vec = Vec::new();
        let mut time0 = LogicalTime::from_nanos(0);

        for (time1, prio) in &self.prio_changes {
            vec.push((time0, *prio));
            time0 = *time1;
        }
        vec.push((time0, self.final_prio));
        vec
    }

    /// Return the priority of the initial timeslice after the thread starts running.
    pub fn initial_priority(&self) -> Priority {
        if let Some((_ns, pr)) = self.prio_changes.first() {
            *pr
        } else {
            self.final_prio
        }
    }
}

impl Default for ThreadHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// An iterator over the preemption history of a thread.  Does not include the final
/// timeslice, which does not have an ending time.
// TODO: in the future this will perform "lazy IO" and pull from disk in batches.
#[derive(PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct ThreadHistoryIterator {
    /// For now we store the full history in memory:
    full_history: ThreadHistory,
    /// Index that tracks our position.
    ix: usize,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[serde(default)]
    chaos_epoch_ix: usize,
}

impl ThreadHistoryIterator {
    /// Return the priority of the initial slice after the thread starts running.
    pub fn initial_priority(&self) -> Priority {
        self.full_history.initial_priority()
    }

    /// Return the priority of the last slice after the last preemption.
    pub fn final_priority(&self) -> Priority {
        self.full_history.final_prio
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Consume recorded epoch transitions up to `current_time`, returning a
    /// transition only when the active recorded factor changed.
    pub fn advance_chaos_epoch(
        &mut self,
        current_time: LogicalTime,
    ) -> Option<ChaosEpochTransition> {
        let mut changed = None;
        while let Some(transition) = self.full_history.chaos_epochs.get(self.chaos_epoch_ix)
            && transition.logical_time <= current_time
        {
            changed = Some(*transition);
            self.chaos_epoch_ix += 1;
        }
        changed
    }

    /// Whether this artifact carries recorded slowdown epochs.
    pub fn has_chaos_epochs(&self) -> bool {
        !self.full_history.chaos_epochs.is_empty()
    }

    /// Whether any recorded timeslice of this thread ends past its branch
    /// target (`--chaos-instruction-offset`), which only a stop at that
    /// target can step to.
    pub fn has_instruction_offsets(&self) -> bool {
        self.full_history.has_instruction_offsets()
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Return the next recorded logical boundary and its exact RCB target when
    /// the artifact was produced by a version that records one.
    pub fn next_with_rcbs(&mut self) -> Option<(LogicalTime, Priority, Option<u64>)> {
        self.next_with_offset()
            .map(|(time, priority, rcbs, _)| (time, priority, rcbs))
    }

    /// [`Self::next_with_rcbs`], with where the recorded timeslice's timer
    /// was armed if `--chaos-instruction-offset` delivered it past its branch
    /// target.
    pub fn next_with_offset(
        &mut self,
    ) -> Option<(
        LogicalTime,
        Priority,
        Option<u64>,
        Option<InstructionOffset>,
    )> {
        let ix = self.ix;
        self.next().map(|(time, priority)| {
            (
                time,
                priority,
                self.full_history.preemption_rcbs.get(ix).copied(),
                self.full_history
                    .preemption_instruction_offsets
                    .get(ix)
                    .copied()
                    .flatten(),
            )
        })
    }

    /// How many recorded rows have been installed as slice ends. The row the
    /// current slice ends is the last of them: `next_with_rcbs` advances past
    /// a row as it installs it, before that row's priority request is filed.
    pub fn installed_rows(&self) -> usize {
        self.ix
    }
}

impl Iterator for ThreadHistoryIterator {
    type Item = (LogicalTime, Priority);

    fn next(&mut self) -> Option<Self::Item> {
        let vec = &self.full_history.prio_changes;
        if vec.len() > self.ix {
            let elt = vec[self.ix];
            self.ix += 1;
            Some(elt)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::schedule::Op;
    use pretty_assertions::assert_eq;
    use test_case::test_case;

    use super::*;
    use crate::types::RcbTimeMultiplier;

    /// A record written by a replay that left a change point out of it says
    /// so, with the global event count where it first did; records derived
    /// from it keep the mark; a record with nothing left out has none.
    #[test]
    fn marked_record_serialization_requires_an_envelope() {
        let ordinary = PreemptionWriter::new(None).into_string();
        assert_eq!(ordinary, "{\"per_thread\":{},\"global\":[],\"rejoins\":[]}");
        let mut writer = PreemptionWriter::new(None);
        writer.mark_replay_departed(7);
        let text = writer.into_string();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            value.get("per_thread").is_none(),
            "old readers must not accept {text}"
        );
        assert!(
            value.get("global").is_none(),
            "old readers must not accept {text}"
        );
        assert_eq!(value["incomplete_preemption_record"]["version"], 1);
        assert_eq!(
            value["incomplete_preemption_record"]["replay_departed_at"],
            7
        );
        let back: PreemptionRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back.replay_departed_at(), Some(7));
    }

    #[test]
    fn marked_records_refuse_binary_serialization_before_writing() {
        let mut writer = PreemptionWriter::new(None);
        writer.mark_replay_departed(7);
        let record: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
        let mut bytes = Vec::new();
        let result =
            bincode::serde::encode_into_std_write(&record, &mut bytes, bincode::config::legacy());
        assert!(
            result.is_err(),
            "a marked record has no unversioned binary encoding"
        );
        assert!(bytes.is_empty(), "refuse before writing a partial record");
    }

    /// A record that marks a timeslice end taken without a stop is format 5,
    /// the first above `BACKGROUND_STARTS_FORMAT`. A writer that notes its
    /// run's background starts keeps the marks: a recording's, and a
    /// preemption replay's re-record whatever format it follows. A writer that
    /// notes none (a schedule replay's re-record) records such an end as an
    /// ordinary one and stays below format 5. A record read with a mark in an
    /// older format is refused, as is a format this Hermit does not know (one
    /// past `PREEMPTION_RECORD_FORMAT_MAX`).
    /// So a Hermit that reads formats 1 to 4 refuses a record with marks,
    /// where it would replay those ends with a stop it does not find.
    #[test]
    fn stopless_slice_ends_need_format_5() {
        let tid = DetTid::from_raw(3);
        let marked = SchedEvent::branches(tid, 100).taken_without_stop();
        const { assert!(PREEMPTION_RECORD_FORMAT_MAX >= STOPLESS_SLICE_ENDS_FORMAT) };
        assert_eq!(STOPLESS_SLICE_ENDS_FORMAT, BACKGROUND_STARTS_FORMAT + 1);
        assert_eq!(
            PREEMPTION_RECORD_FORMAT_WITHOUT_OFFSETS,
            STOPLESS_SLICE_ENDS_FORMAT
        );
        assert_eq!(PREEMPTION_RECORD_FORMAT_MAX, INSTRUCTION_OFFSET_ROWS_FORMAT);
        let written = |mut writer: PreemptionWriter| {
            writer.insert_schedevent(SchedEvent::branches(tid, 100));
            writer.insert_schedevent(marked.clone());
            let record: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
            record.validate_format().unwrap();
            let marks: Vec<bool> = record.global.iter().map(|event| event.stopless).collect();
            (record.format_version, marks)
        };
        for followed in [
            Some(PREEMPTION_RECORD_FORMAT_MAX),
            Some(BACKGROUND_STARTS_FORMAT),
            Some(SWITCH_IN_CHARGE_FORMAT),
        ] {
            assert_eq!(
                written(
                    PreemptionWriter::new(None)
                        .with_format(followed)
                        .noting_background_starts()
                ),
                (Some(STOPLESS_SLICE_ENDS_FORMAT), vec![false, true]),
                "a writer noting its starts, following {followed:?}"
            );
        }
        for followed in [
            Some(PREEMPTION_RECORD_FORMAT_MAX),
            Some(SWITCH_IN_CHARGE_FORMAT),
            None,
        ] {
            let (format, marks) = written(PreemptionWriter::new(None).with_format(followed));
            assert!(
                format.is_none_or(|format| format < STOPLESS_SLICE_ENDS_FORMAT),
                "a writer noting no starts, following {followed:?}: {format:?}"
            );
            assert_eq!(marks, vec![false, false], "following {followed:?}");
        }
        let record = PreemptionRecord::from_sched_events(vec![marked]);
        for format in [
            None,
            Some(TIME_SKIPS_FORMAT),
            Some(SWITCH_IN_CHARGE_FORMAT),
            Some(BACKGROUND_STARTS_FORMAT),
        ] {
            let mut read = record.clone();
            read.format_version = format;
            let error = read.validate_format().unwrap_err();
            assert!(
                error.contains("taken without a stop"),
                "{format:?}: {error}"
            );
        }
        let mut read = record;
        read.format_version = Some(STOPLESS_SLICE_ENDS_FORMAT);
        read.validate_format().unwrap();
        read.format_version = Some(PREEMPTION_RECORD_FORMAT_MAX + 1);
        assert!(read.validate_format().is_err());
    }

    /// A `stopless` mark that contradicts its event is refused wherever a
    /// record is built, written or read: on an end that holds a RIP (the
    /// RIP a stop reads, which an end taken without one does not have) and
    /// on an event that is not a Branch. The builder refuses a RIP on a marked
    /// end, a writer that keeps marks refuses either shape, and the public
    /// reader refuses both records with differing known RIPs and the record
    /// with a marked syscall by name, as a record-format refusal, before any
    /// alignment could leave those RIPs out. The valid shapes, a marked end
    /// with no RIP and an unmarked end with a RIP, are read.
    #[test]
    fn contradictory_stopless_marks_are_refused_where_records_are_built_written_and_read() {
        use std::panic::AssertUnwindSafe;
        use std::panic::catch_unwind;

        use reverie::syscalls::Sysno;

        use crate::types::SyscallPhase;

        let tid = DetTid::from_raw(3);
        let rip = |at| crate::types::InstructionPointer::new(at).unwrap();
        let marked_at = |at| SchedEvent {
            end_rip: Some(rip(at)),
            ..SchedEvent::branches(tid, 100).taken_without_stop()
        };
        let marked_syscall = SchedEvent {
            stopless: true,
            ..SchedEvent::syscall(tid, Sysno::getppid, SyscallPhase::Prehook)
        };

        assert!(
            catch_unwind(|| SchedEvent::branches(tid, 100)
                .taken_without_stop()
                .with_end_rip(rip(0x401000)))
            .is_err()
        );
        for event in [marked_at(0x401000), marked_syscall.clone()] {
            let mut writer = PreemptionWriter::new(None).noting_background_starts();
            assert!(
                catch_unwind(AssertUnwindSafe(|| writer.insert_schedevent(event.clone()))).is_err(),
                "{event:?}"
            );
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.json");
        let read = |event: SchedEvent| {
            let mut record = PreemptionRecord::from_sched_events(vec![event]);
            record.format_version = Some(PREEMPTION_RECORD_FORMAT_MAX);
            std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
            PreemptionReader::try_new(&path).map(|_| ())
        };
        for (event, refusal) in [
            (marked_at(0x401000), "has no end RIP"),
            (marked_at(0x402000), "has no end RIP"),
            (marked_syscall, "only the branches"),
        ] {
            let error = read(event.clone()).expect_err("a contradictory mark is read");
            assert!(
                error.chain().any(|cause| cause.is::<RecordFormatRefusal>()),
                "{event:?}: {error:#}"
            );
            let message = format!("{error:#}");
            assert!(
                message.contains("record event 0") && message.contains(refusal),
                "{event:?}: {message}"
            );
        }
        read(SchedEvent::branches(tid, 100).taken_without_stop()).unwrap();
        read(SchedEvent::branches(tid, 100).with_end_rip(rip(0x401000))).unwrap();
    }

    /// A record that keeps no background starts keeps no `stopless` marks:
    /// lowered below `BACKGROUND_STARTS_FORMAT`, to format 3, it drops them
    /// and reads back, and its replay stops at those ends. A prefix of part
    /// of the schedule, an edit, a mixed schedule and a shared trial format
    /// each lower it; a prefix of the whole schedule keeps its format and its
    /// marks.
    #[test]
    fn a_record_that_keeps_no_background_starts_keeps_no_stopless_marks() {
        let tid = DetTid::from_raw(3);
        let events = vec![
            SchedEvent::branches(tid, 10).taken_without_stop(),
            SchedEvent::branches(tid, 20),
        ];
        let mut record = PreemptionRecord::from_sched_events(events.clone());
        record.format_version = Some(STOPLESS_SLICE_ENDS_FORMAT);
        record.background_starts.push(BackgroundStart {
            after: 1,
            thread: tid,
            call: 1,
        });
        record.validate().unwrap();
        let reads_back = |record: &PreemptionRecord, what: &str| {
            let back: PreemptionRecord =
                serde_json::from_str(&serde_json::to_string(record).unwrap()).unwrap();
            back.validate_format()
                .unwrap_or_else(|error| panic!("{what}: {error}"));
            back.validate()
                .unwrap_or_else(|error| panic!("{what}: {error}"));
            let marks = back.global.iter().filter(|event| event.stopless).count();
            (back.format_version, marks)
        };
        let lowered = (Some(BACKGROUND_STARTS_FORMAT - 1), 0);
        assert_eq!(
            reads_back(&record.schedule_prefix(2), "a prefix of the whole schedule"),
            (Some(STOPLESS_SLICE_ENDS_FORMAT), 1)
        );
        assert_eq!(
            reads_back(&record.schedule_prefix(1), "a shorter prefix"),
            lowered
        );
        let mut edited = record.clone();
        edited.schedevents_iter_mut().for_each(|_| ());
        assert_eq!(reads_back(&edited, "an edit"), lowered);
        let mixed = record
            .interpolate_events(&record, events.iter().rev().cloned().collect())
            .unwrap();
        assert_eq!(reads_back(&mixed, "a mixed schedule"), lowered);
        // Analyze and bisect trials state one shared format; one below the
        // starts' (a trial built from a schedule, which holds none) drops the
        // marks too.
        let mut schedule = record.clone();
        schedule.background_starts.clear();
        let trial = schedule.with_format(format_without_background_starts(record.format_version));
        assert_eq!(reads_back(&trial, "a shared trial format"), lowered);
    }

    /// A `stopless` end the guest reached a same-thread check-in from,
    /// with no branch between, is invisible without its mark: lowering
    /// such a record below [`STOPLESS_SLICE_ENDS_FORMAT`] is refused
    /// wherever the record is validated, rather than silently dropping
    /// the slice end (its grant and virtual-time charge). The reachable
    /// sequence is a marked Branch, the check-in's OtherInstructions,
    /// and its syscall: catch-up records the marked Branch, and the
    /// syscall prehook records the OtherInstructions with no further
    /// branch. A writer below the format that records the same sequence
    /// is refused at flush for the same reason.
    #[test]
    fn a_lowering_that_would_hide_a_stopless_end_is_refused() {
        let tid = DetTid::from_raw(3);
        let check_in = SchedEvent {
            dettid: tid,
            op: Op::OtherInstructions,
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: None,
            stopless: false,
        };
        let events = vec![
            SchedEvent::branches(tid, 123).taken_without_stop(),
            check_in,
            SchedEvent::branches(tid, 7),
        ];
        let mut record = PreemptionRecord::from_sched_events(events);
        record.format_version = Some(PREEMPTION_RECORD_FORMAT_MAX);
        record.validate().unwrap();

        let lowered = record
            .clone()
            .with_format(Some(BACKGROUND_STARTS_FORMAT - 1));
        let refusal = lowered.validate().unwrap_err();
        assert!(
            refusal.contains("`stopless`"),
            "lowering must refuse, not hide the end: {refusal}"
        );
        assert!(
            lowered.global[0].stopless,
            "the mark is kept so the refusal names it"
        );
        // The same refusal through a shorter prefix and an edit.
        assert!(record.schedule_prefix(2).validate().is_err());
        let mut edited = record.clone();
        edited.schedevents_iter_mut().for_each(|_| ());
        assert!(edited.validate().is_err());

        // A writer below the format (which notes no background starts)
        // recording the same sequence is refused at flush.
        let mut writer = PreemptionWriter::new(None);
        writer.insert_schedevent(SchedEvent::branches(tid, 123).taken_without_stop());
        writer.insert_schedevent(SchedEvent {
            dettid: tid,
            op: Op::OtherInstructions,
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: None,
            stopless: false,
        });
        let refusal = writer
            .serialized_string()
            .expect_err("a writer that would hide the end must refuse");
        assert!(refusal.contains("`stopless`"), "{refusal}");

        // Control: the same end followed by a Branch stays visible
        // without its mark, so lowering it is accepted and replayable
        // with a stop.
        let visible = vec![
            SchedEvent::branches(tid, 123).taken_without_stop(),
            SchedEvent::branches(tid, 7),
        ];
        let mut record = PreemptionRecord::from_sched_events(visible);
        record.format_version = Some(PREEMPTION_RECORD_FORMAT_MAX);
        let lowered = record.with_format(Some(BACKGROUND_STARTS_FORMAT - 1));
        lowered.validate().unwrap();
        assert!(!lowered.global[0].stopless);
    }

    #[test]
    fn a_departed_replays_record_says_it_is_not_exact() {
        let mut writer = PreemptionWriter::new(None);
        writer.mark_replay_departed(7);
        writer.mark_replay_departed(9);
        let written = writer.into_string();
        let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
        assert_eq!(back.replay_departed_at(), Some(7), "the first departure");
        assert_eq!(back.clone_preemptions_only().replay_departed_at(), Some(7));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        std::fs::write(&path, &written).unwrap();
        assert_eq!(read_recorded_replay_departure(&path), Ok(Some(7)));
        let exact = PreemptionWriter::new(None).into_string();
        assert!(!exact.contains("replay_departed_at"), "{exact}");
        std::fs::write(&path, &exact).unwrap();
        assert_eq!(read_recorded_replay_departure(&path), Ok(None));
    }

    /// A writer that notes background starts stamps the current format, even
    /// on a record without any, and notes each start after the events recorded
    /// so far; one that does not note them keeps format 1 and takes none. A
    /// record's starts must be in order, within its events and of a format
    /// that notes them, and an edit of the schedule drops them with the
    /// format.
    #[test]
    fn background_starts_follow_their_events_and_version_the_record() {
        let event = |tid: i32| SchedEvent {
            dettid: DetTid::from_raw(tid),
            op: Op::OtherInstructions,
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: Some(LogicalTime::from_nanos(5)),
            stopless: false,
        };
        let read = |writer: PreemptionWriter| -> PreemptionRecord {
            serde_json::from_str(&writer.into_string()).unwrap()
        };
        let mut plain = PreemptionWriter::new(None);
        plain.insert_schedevent(event(3));
        plain.insert_background_start(DetTid::from_raw(3), 1);
        let plain_json = plain.into_string();
        assert!(!plain_json.contains("format_version"), "{plain_json}");
        assert!(!plain_json.contains("background_starts"), "{plain_json}");

        let mut none = PreemptionWriter::new(None).noting_background_starts();
        none.insert_schedevent(event(3));
        let none = read(none);
        assert_eq!(none.format_version, Some(STOPLESS_SLICE_ENDS_FORMAT));
        assert!(none.notes_background_starts());
        assert!(none.background_starts().is_empty());

        let mut writer = PreemptionWriter::new(None).noting_background_starts();
        writer.insert_schedevent(event(3));
        writer.insert_background_start(DetTid::from_raw(3), 7);
        writer.insert_schedevent(event(5));
        writer.insert_background_start(DetTid::from_raw(5), 2);
        writer.insert_background_start(DetTid::from_raw(3), 8);
        let record = read(writer);
        let start = |after, tid, call| BackgroundStart {
            after,
            thread: DetTid::from_raw(tid),
            call,
        };
        assert_eq!(
            record.background_starts(),
            [start(1, 3, 7), start(2, 5, 2), start(2, 3, 8)]
        );
        record.validate().unwrap();

        let mut out_of_order = record.clone();
        out_of_order.background_starts.swap(0, 1);
        assert!(out_of_order.validate().is_err());
        let mut past_the_end = record.clone();
        past_the_end.background_starts.push(start(3, 5, 3));
        assert!(past_the_end.validate().is_err());
        let mut unnoted = record.clone();
        unnoted.format_version = None;
        assert!(unnoted.validate().is_err());
        let mut newer = record.clone();
        newer.format_version = Some(PREEMPTION_RECORD_FORMAT_MAX + 1);
        assert!(newer.validate().is_err());

        let mut edited = record;
        edited.preemptions_only();
        assert!(edited.background_starts().is_empty());
        assert!(!edited.notes_background_starts());
    }

    /// Only a writer that notes every background start states
    /// `BACKGROUND_STARTS_FORMAT` or newer: any other writer, an edit, a
    /// mixed interpolation and `with_current_format` on a record without
    /// noted starts state the format below it, and lowering never raises an
    /// older format.
    #[test]
    fn only_a_record_that_notes_its_background_starts_states_their_format() {
        let below = Some(BACKGROUND_STARTS_FORMAT - 1);
        assert_eq!(
            format_without_background_starts(Some(PREEMPTION_RECORD_FORMAT_MAX)),
            below
        );
        assert_eq!(
            format_without_background_starts(Some(TIME_SKIPS_FORMAT)),
            Some(TIME_SKIPS_FORMAT)
        );
        assert_eq!(format_without_background_starts(None), None);
        let tid = DetTid::from_raw(3);
        let read = |writer: PreemptionWriter| -> PreemptionRecord {
            serde_json::from_str(&writer.into_string()).unwrap()
        };

        let mut plain = PreemptionWriter::new(None).with_format(Some(PREEMPTION_RECORD_FORMAT_MAX));
        plain.insert_schedevent(SchedEvent::branches(tid, 10));
        plain.insert_background_start(tid, 1);
        let plain = read(plain);
        assert_eq!(plain.format_version(), below);
        assert!(!plain.notes_background_starts());
        assert!(plain.background_starts().is_empty());

        let mut noting = PreemptionWriter::new(None)
            .with_format(Some(PREEMPTION_RECORD_FORMAT_MAX))
            .noting_background_starts();
        noting.insert_schedevent(SchedEvent::branches(tid, 10));
        noting.insert_background_start(tid, 1);
        noting.insert_schedevent(SchedEvent::branches(tid, 20));
        let noting = read(noting);
        assert_eq!(noting.format_version(), Some(STOPLESS_SLICE_ENDS_FORMAT));
        assert!(noting.notes_background_starts());
        assert_eq!(noting.background_starts().len(), 1);
        assert_eq!(
            noting.clone().with_current_format().format_version(),
            Some(STOPLESS_SLICE_ENDS_FORMAT)
        );
        assert_eq!(
            PreemptionRecord::from_sched_events(Vec::new())
                .with_current_format()
                .format_version(),
            below
        );

        let mut edited = noting.clone();
        edited.split_map(|event, _| [event]);
        assert_eq!(edited.format_version(), below);
        assert!(edited.background_starts().is_empty());
        edited.validate().unwrap();
        let mut older = PreemptionRecord::from_sched_events(vec![SchedEvent::branches(tid, 10)])
            .with_format(Some(TIME_SKIPS_FORMAT));
        older.preemptions_only();
        assert_eq!(older.format_version(), Some(TIME_SKIPS_FORMAT));

        let other_tid = DetTid::from_raw(5);
        let events = |order: &[(DetTid, u32)]| -> Vec<SchedEvent> {
            order
                .iter()
                .map(|&(tid, count)| SchedEvent::branches(tid, count))
                .collect()
        };
        let source = |order: &[(DetTid, u32)], after: u64| {
            let mut record = PreemptionRecord::from_sched_events(events(order));
            record.format_version = Some(BACKGROUND_STARTS_FORMAT);
            record.background_starts.push(BackgroundStart {
                after,
                thread: tid,
                call: 1,
            });
            record.validate().unwrap();
            record
        };
        let a = source(&[(tid, 10), (other_tid, 20), (tid, 30)], 1);
        let b = source(&[(other_tid, 20), (tid, 10), (tid, 30)], 2);
        let mixed = a
            .interpolate_events(&b, events(&[(tid, 10), (tid, 30), (other_tid, 20)]))
            .unwrap();
        assert_eq!(mixed.format_version(), below);
        assert!(mixed.background_starts().is_empty());
        assert_eq!(
            a.interpolate_events(&b, a.schedevents().to_vec())
                .unwrap()
                .format_version(),
            Some(BACKGROUND_STARTS_FORMAT)
        );
    }

    /// A mixed interpolation keeps no readmissions: their turns name points
    /// of an event order the result never ran, so its replay would readmit
    /// calls where neither endpoint's run did. Two endpoints with the same
    /// log and different event orders give a mixed record with none, as an
    /// edit does (`forget_run_points`), and selecting an endpoint's exact
    /// order keeps that record whole, its log included.
    #[test]
    fn a_mixed_interpolation_keeps_no_readmissions() {
        let (tid, other_tid) = (DetTid::from_raw(3), DetTid::from_raw(5));
        let events = |order: &[(DetTid, u32)]| -> Vec<SchedEvent> {
            order
                .iter()
                .map(|&(tid, count)| SchedEvent::branches(tid, count))
                .collect()
        };
        let log = vec![crate::scheduler::rejoin_log::Rejoin {
            turn: 2,
            wakes: 0,
            calls: vec![(tid, 1)],
            popped: Some(Vec::new()),
        }];
        let source = |order: &[(DetTid, u32)]| {
            let mut record = PreemptionRecord::from_sched_events(events(order));
            record.format_version = Some(SWITCH_IN_CHARGE_FORMAT);
            record.rejoins = Some(log.clone());
            record.validate().unwrap();
            record
        };
        let a = source(&[(tid, 10), (other_tid, 20), (tid, 30)]);
        let b = source(&[(other_tid, 20), (tid, 10), (tid, 30)]);
        let mixed = a
            .interpolate_events(&b, events(&[(tid, 10), (tid, 30), (other_tid, 20)]))
            .unwrap();
        assert_eq!(mixed.rejoins, None);
        for (endpoint, record) in [(&a, a.clone()), (&b, b.clone())] {
            let exact = a
                .interpolate_events(&b, endpoint.schedevents().to_vec())
                .unwrap();
            assert_eq!(exact.rejoins, Some(log.clone()));
            assert_eq!(exact, record);
        }
    }

    /// The starts cap lowers only a format this Hermit reads at or above
    /// `BACKGROUND_STARTS_FORMAT`. A newer format, and an invalid 0, stay as
    /// stated through a schedule replay's re-record (the writer), a shorter
    /// prefix, an edit and `with_current_format`, and an interpolation refuses
    /// them, so that reading still refuses the record by name: lowered, a
    /// newer Hermit's record would read as one without what it may hold. A
    /// plain `min` in `format_without_background_starts` fails this test.
    #[test]
    fn a_format_this_hermit_does_not_read_survives_the_starts_cap() {
        let tid = DetTid::from_raw(3);
        for stated in [
            PREEMPTION_RECORD_FORMAT_MAX + 1,
            PREEMPTION_RECORD_FORMAT_MAX + 2,
            0,
        ] {
            assert_eq!(format_without_background_starts(Some(stated)), Some(stated));
            let refusal = PreemptionRecord::from_sched_events(Vec::new())
                .with_format(Some(stated))
                .validate_format()
                .unwrap_err();
            assert!(
                refusal.contains(&format!("record format Some({stated})")),
                "{refusal}"
            );
            let refused = |record: &PreemptionRecord, path: &str| {
                assert_eq!(record.format_version(), Some(stated), "{path}");
                let back: PreemptionRecord =
                    serde_json::from_str(&serde_json::to_string(record).unwrap()).unwrap();
                assert_eq!(back.format_version(), Some(stated), "{path}");
                assert_eq!(back.validate_format(), Err(refusal.clone()), "{path}");
            };
            let mut writer = PreemptionWriter::new(None).with_format(Some(stated));
            writer.insert_schedevent(SchedEvent::branches(tid, 10));
            let written: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
            refused(&written, "schedule replay re-record");
            let mut record = PreemptionRecord::from_sched_events(vec![
                SchedEvent::branches(tid, 10),
                SchedEvent::branches(tid, 20),
            ]);
            record.format_version = Some(stated);
            refused(&record.schedule_prefix(1), "shorter prefix");
            let mut edited = record.clone();
            edited.split_map(|event, _| [event]);
            refused(&edited, "edit");
            refused(&record.clone().with_current_format(), "with_current_format");
            let mixed = record
                .interpolate_events(&record, record.schedevents()[..1].to_vec())
                .unwrap_err();
            assert_eq!(mixed, refusal, "interpolation");
        }
    }

    #[test]
    fn streamed_files_keep_every_metadata_history_and_callback_field() {
        use callback_history::Origin;
        use callback_history::Site;
        use reverie::syscalls::Sysno;

        use crate::slice_boundary::callbacks::Callback;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("complete.json");
        let tid = DetTid::from_raw(3);
        let empty_tid = DetTid::from_raw(5);
        let epoch = "2001-02-03T04:05:06Z".parse().unwrap();
        let mut writer = PreemptionWriter::new(Some(path.clone()))
            .with_epoch(epoch)
            .with_pipe_wake_latency(Some(0))
            .with_signal_state_copy(false);
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.register_thread(empty_tid, DEFAULT_PRIORITY);
        writer.insert_chaos_epoch(
            tid,
            ChaosEpochTransition {
                logical_time: LogicalTime::from_nanos(50),
                epoch: 0,
                factor: RcbTimeMultiplier::from_f64(1.5),
            },
        );
        writer.insert_reprioritization(
            tid,
            LogicalTime::from_nanos(100),
            10,
            DEFAULT_PRIORITY,
            20,
            None,
        );
        writer.insert_schedevent(SchedEvent::branches(tid, 10));
        writer.insert_callback_checkpoint(
            tid,
            Origin {
                callback: Callback::Syscall(Sysno::close),
                site: Site::Posthook,
                progress: 10,
                last_op: Some(Op::Branch),
                time: LogicalTime::from_nanos(100),
                rcbs: 10,
            },
        );
        let expected = writer.inner.clone();
        let mut expected_bytes = serde_json::to_vec(&expected).unwrap();
        expected_bytes.push(b'\n');
        writer.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), expected_bytes);
        let recording = boundary_recording::Recording::read(&path).unwrap();
        let summary = recording.inspect().unwrap();
        assert_eq!(summary.epoch, Some(epoch));
        assert_eq!(summary.pipe_wake_latency, Some(0));
        assert_eq!(summary.signal_state_copy, Some(false));
        assert_eq!(summary.event_rows, 1);
        assert_eq!(summary.threads.len(), 2);
        assert_eq!(summary.threads[&tid].priority_rows, 1);
        assert_eq!(summary.threads[&tid].rcb_rows, 1);
        assert_eq!(summary.threads[&tid].chaos_epoch_rows, 1);
        assert_eq!(summary.threads[&tid].callback_rows, 1);
        assert_eq!(summary.threads[&empty_tid].priority_rows, 0);
        assert_eq!(summary.threads[&empty_tid].final_priority, DEFAULT_PRIORITY);
        assert_eq!(recording.into_legacy("test").unwrap(), expected);
    }

    #[test]
    fn deprecated_trace_reader_keeps_its_independent_typed_legacy_contract() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.json");
        std::fs::write(&path, br#"{"per_thread":{},"global":[],"extra":1e999}"#).unwrap();
        assert!(read_trace(&path).is_empty());
        assert!(boundary_recording::Recording::read(&path).is_ok());
        std::fs::write(&path, br#"{"per_thread":{},"global":[]} false"#).unwrap();
        assert!(std::panic::catch_unwind(|| read_trace(&path)).is_err());
        std::fs::write(
            &path,
            b"{\"per_thread\":{},\"global\":[],\"extra\":\"\xff\"}",
        )
        .unwrap();
        assert!(std::panic::catch_unwind(|| read_trace(&path)).is_err());
    }

    #[test]
    fn bounded_summary_validates_boundaries_without_writing_or_retrying_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retained.json");
        std::fs::write(&path, b"retained original").unwrap();
        let writer = PreemptionWriter::new(Some(path.clone()))
            .with_slice_boundaries()
            .unwrap();
        assert!(
            writer
                .summary()
                .unwrap()
                .contains("0 boundary-protocol rows")
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"retained original");

        let mut invalid = PreemptionWriter::new(Some(path.clone()))
            .with_slice_boundaries()
            .unwrap();
        invalid
            .inner
            .global
            .push(SchedEvent::branches(DetTid::from_raw(3), 1));
        assert!(invalid.summary().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"retained original");
    }

    /// The bounded summary is compared evidence on the no-destination path,
    /// so histories with identical row counts but different recorded
    /// contents must not collapse to the same description.
    #[test]
    fn bounded_summary_distinguishes_equal_count_histories_by_content() {
        let tid = DetTid::from_raw(3);
        let summary = |time, rcbs, store_exclusive_failures| {
            let mut writer = PreemptionWriter::new(None);
            writer.register_thread(tid, DEFAULT_PRIORITY);
            writer.insert_reprioritization(
                tid,
                LogicalTime::from_nanos(time),
                rcbs,
                DEFAULT_PRIORITY,
                20,
                None,
            );
            writer.record_store_exclusive_failures(store_exclusive_failures);
            writer.summary().unwrap()
        };

        assert_ne!(summary(100, 10, 0), summary(200, 20, 0));
        // A host-observed diagnostic count is not schedule content: it
        // compares (and hashes) as equal in the record itself, so the
        // compared summary evidence must not reintroduce it.
        assert_eq!(summary(100, 10, 0), summary(100, 10, 20));

        let boundary_summary = |store_exclusive_failures| {
            let mut writer = PreemptionWriter::new(None).with_slice_boundaries().unwrap();
            writer.register_thread(tid, DEFAULT_PRIORITY);
            writer.insert_reprioritization(
                tid,
                LogicalTime::from_nanos(100),
                10,
                DEFAULT_PRIORITY,
                20,
                None,
            );
            writer.record_store_exclusive_failures(store_exclusive_failures);
            writer.summary().unwrap()
        };
        assert_eq!(boundary_summary(0), boundary_summary(20));

        // The instruction-offset marks are recorded content too: rows alone
        // and a record that notes no background starts time their rows by
        // other rules, so a record differing only in either mark must not
        // collapse to an unmarked record's summary, on either writer path.
        // (InfoRecord, the INFO mirror in record_serde, carries both marks
        // for this to hold.)
        let marked_summary = |rows_alone: bool, unnoted: bool, boundaries: bool| {
            let mut writer = PreemptionWriter::new(None);
            if boundaries {
                writer = writer.with_slice_boundaries().unwrap();
            }
            writer.register_thread(tid, DEFAULT_PRIORITY);
            writer.insert_reprioritization(
                tid,
                LogicalTime::from_nanos(100),
                10,
                DEFAULT_PRIORITY,
                20,
                None,
            );
            writer.inner.format_version = Some(INSTRUCTION_OFFSET_ROWS_FORMAT);
            writer.inner.rows_alone = rows_alone;
            writer.inner.background_starts_unnoted = unnoted;
            writer.summary().unwrap()
        };
        for boundaries in [false, true] {
            let plain = marked_summary(false, false, boundaries);
            assert_ne!(
                plain,
                marked_summary(true, false, boundaries),
                "rows_alone alone must move the summary (boundaries={boundaries})"
            );
            assert_ne!(
                plain,
                marked_summary(false, true, boundaries),
                "background_starts_unnoted alone must move the summary (boundaries={boundaries})"
            );
        }
    }

    /// Every streaming write/summary path runs the writer's stopless
    /// lowering refusal (`finalize`) before serializing, as the legacy
    /// write path does: a below-format record whose stopless end would
    /// become invisible without its mark is refused before any file is
    /// created or truncated, never streamed out silently losing the end.
    /// An end that stays visible without its mark is lowered and written.
    #[test]
    fn a_below_format_stopless_record_is_refused_before_the_streaming_writer_creates_a_file() {
        let tid = DetTid::from_raw(3);
        let check_in = SchedEvent {
            dettid: tid,
            op: Op::OtherInstructions,
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: None,
            stopless: false,
        };
        let invisible = [
            SchedEvent::branches(tid, 123).taken_without_stop(),
            check_in,
        ];
        for boundaries in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("record.json");
            std::fs::write(&path, b"retained original").unwrap();
            let mut writer = PreemptionWriter::new(Some(path.clone()));
            if boundaries {
                writer = writer.with_slice_boundaries().unwrap();
                // A boundary writer's tape carries slice ends in its own
                // protocol (insert lowers the mark), so put the marked
                // legacy stream where a boundary envelope would carry it:
                // in the metadata history the envelope serializes.
                writer.inner.global.extend(invisible.iter().cloned());
            } else {
                for event in invisible.iter().cloned() {
                    writer.insert_schedevent(event);
                }
            }
            let refusal = writer
                .flush()
                .expect_err("a writer that would hide the end must refuse");
            assert!(
                refusal.contains("`stopless`"),
                "boundaries={boundaries}: {refusal}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"retained original");

            // The summary path refuses the same record without writing.
            let mut writer = PreemptionWriter::new(Some(path.clone()));
            if boundaries {
                writer = writer.with_slice_boundaries().unwrap();
                writer.inner.global.extend(invisible.iter().cloned());
            } else {
                for event in invisible.iter().cloned() {
                    writer.insert_schedevent(event);
                }
            }
            let refusal = writer
                .summary()
                .expect_err("a summary that would hide the end must refuse");
            assert!(
                refusal.contains("`stopless`"),
                "boundaries={boundaries}: {refusal}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"retained original");
        }

        // Control: the same end followed by a Branch stays visible without
        // its mark, so the streaming writer lowers it and writes the file.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("visible.json");
        let mut writer = PreemptionWriter::new(Some(path.clone()));
        writer.insert_schedevent(SchedEvent::branches(tid, 123).taken_without_stop());
        writer.insert_schedevent(SchedEvent::branches(tid, 7));
        writer.flush().unwrap();
        let written = boundary_recording::Recording::read(&path).unwrap();
        let boundary_recording::Recording::Legacy(written) = written else {
            panic!("a legacy writer writes a legacy recording")
        };
        assert!(!written.global[0].stopless);
    }

    /// `--pipe-wake-latency` rides in a preemption record as the epoch does:
    /// omitted when off, kept through JSON and through every record a run or
    /// an analysis writes, and compared exactly, so that `Some(0)` is not off
    /// (https://github.com/rrnewton/hermit/issues/4380).
    #[test]
    fn the_pipe_wake_latency_round_trips_and_is_compared_exactly() {
        let off = PreemptionRecord::from_sched_events(Vec::new());
        assert!(
            !serde_json::to_string(&off)
                .unwrap()
                .contains("pipe_wake_latency")
        );
        let dir = tempfile::tempdir().unwrap();
        for latency in [Some(0), Some(3_000_000)] {
            let record =
                PreemptionRecord::from_sched_events(Vec::new()).with_pipe_wake_latency(latency);
            let json = serde_json::to_string(&record).unwrap();
            let back: PreemptionRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(back.pipe_wake_latency(), latency);
            assert_eq!(record.clone_preemptions_only().pipe_wake_latency(), latency);
            let minimized =
                PreemptionRecord::from_vecs(&BTreeMap::new()).with_pipe_wake_latency(latency);
            assert_eq!(minimized.pipe_wake_latency(), latency);
            let path = dir.path().join("record.json");
            std::fs::write(&path, &json).unwrap();
            assert_eq!(read_recorded_pipe_wake_latency(&path), Ok(latency));
            let written = PreemptionWriter::new(None)
                .with_pipe_wake_latency(latency)
                .into_string();
            assert!(
                written.contains(&format!("\"pipe_wake_latency\":{}", latency.unwrap())),
                "{written}"
            );
        }
        for (recorded, replay, refused) in [
            (None, None, false),
            (Some(0), Some(0), false),
            (Some(0), None, true),
            (None, Some(0), true),
            (Some(0), Some(3_000_000), true),
        ] {
            assert_eq!(
                pipe_wake_latency_mismatch("--replay-schedule-from=x", recorded, replay).is_some(),
                refused,
                "{recorded:?} replayed with {replay:?}"
            );
        }
    }

    /// How a run charged runs of expired timed waits rides in a preemption
    /// record only when it charged them once (`--expired-wait-charge=once`,
    /// by default outside race search), so an `each` record is written
    /// exactly as before; a record that does not say means every expired
    /// wait was charged.
    #[test]
    fn the_expired_wait_charge_rule_round_trips() {
        let unsaid = PreemptionRecord::from_sched_events(Vec::new());
        assert!(!unsaid.expired_wait_runs_charged_once());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        for once in [false, true] {
            let written = PreemptionWriter::new(None)
                .with_expired_wait_runs_charged_once(once)
                .into_string();
            assert_eq!(
                written.contains("expired_wait_runs_charged_once"),
                once,
                "{written}"
            );
            let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(back.expired_wait_runs_charged_once(), once);
            assert_eq!(
                back.clone_preemptions_only()
                    .expired_wait_runs_charged_once(),
                once
            );
            std::fs::write(&path, &written).unwrap();
            assert_eq!(
                read_recorded_expired_wait_runs_charged_once(&path),
                Ok(once)
            );
        }
        std::fs::write(&path, serde_json::to_string(&unsaid).unwrap()).unwrap();
        assert_eq!(
            read_recorded_expired_wait_runs_charged_once(&path),
            Ok(false)
        );
    }

    /// The guest numbering of a run without a PID namespace rides in its
    /// record, and a replay that numbers otherwise is refused; a namespace
    /// run's record is written as before and reads back as namespace-numbered.
    #[test]
    fn the_guest_numbering_round_trips_and_is_compared() {
        use detcore_model::pid::GuestIds;
        let chosen = |base, refuse_host_ids| GuestIds::Chosen {
            base,
            refuse_host_ids,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        let namespace = PreemptionWriter::new(None)
            .with_guest_ids(GuestIds::Namespace)
            .into_string();
        assert!(!namespace.contains("guest_ids"), "{namespace}");
        std::fs::write(&path, &namespace).unwrap();
        assert_eq!(read_recorded_guest_ids(&path), Ok(GuestIds::Namespace));
        for ids in [GuestIds::Host, chosen(4000, false)] {
            let written = PreemptionWriter::new(None)
                .with_guest_ids(ids)
                .into_string();
            let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(back.guest_ids(), Some(ids));
            assert_eq!(back.clone_preemptions_only().guest_ids(), Some(ids));
            std::fs::write(&path, &written).unwrap();
            assert_eq!(read_recorded_guest_ids(&path), Ok(ids));
            // A record analyze writes for a trial states the trials' numbering
            // the same way.
            let stamped = PreemptionRecord::from_sched_events(Vec::new()).with_guest_ids(ids);
            stamped.write_to_disk(&path).unwrap();
            assert_eq!(read_recorded_guest_ids(&path), Ok(ids));
        }
        let unstamped = PreemptionRecord::from_sched_events(Vec::new())
            .with_guest_ids(chosen(4000, true))
            .with_guest_ids(GuestIds::Namespace);
        assert_eq!(unstamped.guest_ids(), None);
        for (recorded, replay, refused) in [
            (GuestIds::Namespace, GuestIds::Namespace, false),
            (chosen(4000, false), chosen(4000, true), false),
            (chosen(4000, false), chosen(4001, false), true),
            (chosen(4000, false), GuestIds::Namespace, true),
            (GuestIds::Namespace, chosen(4000, true), true),
            (GuestIds::Host, GuestIds::Host, false),
            (GuestIds::Host, GuestIds::Namespace, true),
        ] {
            assert_eq!(
                guest_ids_mismatch("--replay-schedule-from=x", recorded, replay).is_some(),
                refused,
                "{recorded:?} replayed with {replay:?}"
            );
        }
    }

    /// How a run registered queued sleeps rides in a preemption record only
    /// when it registered them at the first pass
    /// (`--sleep-registration=first-pass`, by default outside race search),
    /// so a `when-chosen` record is written exactly as before; a record that
    /// does not say means a sleep was registered only when its thread was
    /// chosen.
    #[test]
    fn the_sleep_registration_rule_round_trips() {
        let unsaid = PreemptionRecord::from_sched_events(Vec::new());
        assert!(!unsaid.queued_sleeps_registered_at_first_pass());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        for first_pass in [false, true] {
            let written = PreemptionWriter::new(None)
                .with_queued_sleeps_registered_at_first_pass(first_pass)
                .into_string();
            assert_eq!(
                written.contains("queued_sleeps_registered_at_first_pass"),
                first_pass,
                "{written}"
            );
            let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(back.queued_sleeps_registered_at_first_pass(), first_pass);
            assert_eq!(
                back.clone_preemptions_only()
                    .queued_sleeps_registered_at_first_pass(),
                first_pass
            );
            std::fs::write(&path, &written).unwrap();
            assert_eq!(
                read_recorded_queued_sleeps_registered_at_first_pass(&path),
                Ok(first_pass)
            );
        }
        std::fs::write(&path, serde_json::to_string(&unsaid).unwrap()).unwrap();
        assert_eq!(
            read_recorded_queued_sleeps_registered_at_first_pass(&path),
            Ok(false)
        );
    }

    /// The post-crash bound and switch ride in a preemption record (absent,
    /// and not written, when the switch is off); a replay takes a recorded
    /// bound when it sets none, and refuses another bound or a bound for a
    /// record made with the switch off.
    #[test]
    fn the_post_crash_switch_round_trips_and_a_replay_takes_the_recorded_bound() {
        let switch = crate::post_crash::PostCrashSwitch {
            dtid: DetTid::from_raw(5),
            signal: libc::SIGABRT,
            exec_by: Some(DetTid::from_raw(9)),
            turn: 42,
            bound: 200_000_000,
        };
        let mut writer = PreemptionWriter::new(None)
            .with_post_crash_timeslice(Some(200_000_000))
            .noting_post_crash_switches();
        writer.push_post_crash(switch);
        let written = writer.into_string();
        let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
        assert_eq!(back.post_crash_timeslice(), Some(200_000_000));
        assert_eq!(back.post_crash(), &[switch]);
        assert!(
            back.clone_preemptions_only()
                .post_crash_complete()
                .is_none()
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        std::fs::write(&path, &written).unwrap();
        assert_eq!(
            read_recorded_post_crash_timeslice(&path),
            Ok(Some(200_000_000))
        );
        assert_eq!(
            read_recorded_post_crash_switches(&path),
            Ok(Some(vec![switch]))
        );
        // A run with the switch on that took none lists none; a record
        // stamped with the bound alone lists nothing at all.
        let none_taken = PreemptionWriter::new(None)
            .with_post_crash_timeslice(Some(200_000_000))
            .noting_post_crash_switches()
            .into_string();
        std::fs::write(&path, &none_taken).unwrap();
        assert_eq!(
            read_recorded_post_crash_switches(&path),
            Ok(Some(Vec::new()))
        );
        let stamped = PreemptionWriter::new(None)
            .with_post_crash_timeslice(Some(200_000_000))
            .into_string();
        std::fs::write(&path, &stamped).unwrap();
        assert_eq!(read_recorded_post_crash_switches(&path), Ok(None));
        let off = PreemptionWriter::new(None).into_string();
        assert!(!off.contains("post_crash"), "{off}");
        std::fs::write(&path, &off).unwrap();
        assert_eq!(read_recorded_post_crash_timeslice(&path), Ok(None));
        assert_eq!(read_recorded_post_crash_switches(&path), Ok(None));

        let source = "--replay-preemptions-from=x";
        // Unset: the record decides.
        assert_eq!(
            post_crash_timeslice_for_replay(source, None, None),
            Ok(None)
        );
        assert_eq!(
            post_crash_timeslice_for_replay(source, Some(7), None),
            Ok(Some(7))
        );
        // Given and in effect: it must be the record's.
        assert_eq!(
            post_crash_timeslice_for_replay(source, Some(7), Some(Some(7))),
            Ok(Some(7))
        );
        for (recorded, replay) in [(Some(7), Some(8)), (None, Some(8))] {
            let refusal =
                post_crash_timeslice_for_replay(source, recorded, Some(replay)).unwrap_err();
            assert!(refusal.starts_with(source), "{refusal}");
            assert!(refusal.contains("--post-crash-timeslice=8"), "{refusal}");
        }
        // Given but not in effect (not above --max-timeslice): the switch is
        // off, which a record made with it on refuses and one made without it
        // accepts.
        assert_eq!(
            post_crash_timeslice_for_replay(source, None, Some(None)),
            Ok(None)
        );
        let refusal = post_crash_timeslice_for_replay(source, Some(7), Some(None)).unwrap_err();
        assert!(refusal.contains("the post-crash switch off"), "{refusal}");
    }

    /// The signal-state copy setting rides in a preemption record as the
    /// pipe-wake latency does, and a replay under the other setting is
    /// refused; a record that does not say (older, or synthesized) is not.
    /// `--chaos-timeslice-draw=run` draws a pure function of the scheduler
    /// seed, inside the range, spread evenly over its decades (log-uniform),
    /// and a record stores the draw only when there is one.
    #[test]
    fn a_drawn_chaos_timeslice_is_seeded_log_uniform_and_recorded() {
        let range: detcore_model::config::TimesliceRange = "1000..1000000000".parse().unwrap();
        let mut decades = [0u32; 6];
        for seed in 0..3000 {
            let drawn = draw_chaos_timeslice(range, seed).get();
            assert_eq!(draw_chaos_timeslice(range, seed).get(), drawn);
            assert!((1_000..=1_000_000_000).contains(&drawn), "{drawn}");
            decades[((drawn as f64).log10() as usize - 3).min(5)] += 1;
        }
        // 500 expected in each decade.
        assert!(
            decades.iter().all(|count| (400..=600).contains(count)),
            "{decades:?}"
        );
        assert_ne!(
            draw_chaos_timeslice(range, 1),
            draw_chaos_timeslice(range, 2)
        );
        let point = "4321..4321".parse().unwrap();
        assert_eq!(draw_chaos_timeslice(point, 9).get(), 4321);

        let unsaid = PreemptionWriter::new(None).into_string();
        assert!(!unsaid.contains("chaos_timeslice"), "{unsaid}");
        let written = PreemptionWriter::new(None)
            .with_chaos_timeslice(std::num::NonZeroU64::new(4321))
            .into_string();
        assert!(written.contains("\"chaos_timeslice\":4321"), "{written}");
        let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
        assert_eq!(back.chaos_timeslice(), Some(4321));
        assert_eq!(back.clone_preemptions_only().chaos_timeslice(), Some(4321));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        std::fs::write(&path, &written).unwrap();
        assert_eq!(read_recorded_chaos_timeslice(&path), Ok(Some(4321)));
        std::fs::write(&path, &unsaid).unwrap();
        assert_eq!(read_recorded_chaos_timeslice(&path), Ok(None));
    }

    #[test]
    fn the_in_memory_exclusive_rewrite_round_trips_and_is_compared() {
        let off = PreemptionWriter::new(None).into_string();
        assert!(!off.contains("rewrite_exclusives_in_memory"), "{off}");
        let on = PreemptionWriter::new(None)
            .with_rewrite_exclusives_in_memory(true)
            .into_string();
        assert!(on.contains("\"rewrite_exclusives_in_memory\":true"), "{on}");
        let back: PreemptionRecord = serde_json::from_str(&on).unwrap();
        assert!(back.rewrite_exclusives_in_memory());
        assert!(back.clone_preemptions_only().rewrite_exclusives_in_memory());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.json");
        std::fs::write(&path, &on).unwrap();
        assert_eq!(read_recorded_rewrite_exclusives_in_memory(&path), Ok(true));
        std::fs::write(&path, &off).unwrap();
        assert_eq!(read_recorded_rewrite_exclusives_in_memory(&path), Ok(false));
        for (recorded, replay) in [(false, false), (true, true), (true, false), (false, true)] {
            assert_eq!(
                rewrite_exclusives_in_memory_mismatch("--replay-schedule-from=x", recorded, replay)
                    .is_some(),
                recorded != replay,
                "{recorded} replayed with {replay}"
            );
        }
    }

    #[test]
    fn the_exclusive_rewrite_provenance_round_trips_and_is_compared() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.json");
        for mode in [
            RewriteExclusivesMode::Overlay,
            RewriteExclusivesMode::Memory,
        ] {
            let rewrite = RewriteExclusives::current(mode);
            let written = PreemptionWriter::new(None)
                .with_rewrite_exclusives(Some(rewrite))
                .into_string();
            let name = match mode {
                RewriteExclusivesMode::Overlay => "overlay",
                RewriteExclusivesMode::Memory => "memory",
            };
            assert!(
                written.contains(&format!(
                    "\"rewrite_exclusives\":{{\"mode\":\"{name}\",\"format_version\":{}}}",
                    crate::aarch64_lse::REWRITE_FORMAT_VERSION
                )),
                "{written}"
            );
            // The older field agrees, so a pre-provenance reader still
            // refuses the in-memory mismatch it can see.
            assert_eq!(
                written.contains("\"rewrite_exclusives_in_memory\":true"),
                rewrite.in_memory(),
                "{written}"
            );
            let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(back.rewrite_exclusives(), Some(rewrite));
            assert_eq!(
                back.clone_preemptions_only().rewrite_exclusives(),
                Some(rewrite)
            );
            std::fs::write(&path, &written).unwrap();
            assert_eq!(
                read_recorded_rewrite_exclusives(&path),
                Ok(RecordedRewriteExclusives {
                    rewrite: Some(rewrite),
                    in_memory: rewrite.in_memory(),
                    stripped_heuristic: false,
                })
            );
        }
        // A record that states nothing, one that states only the older
        // in-memory field, and one of the removed heuristic, read as what
        // they are.
        for (json, expected) in [
            (
                r#"{"per_thread":{},"global":[]}"#,
                RecordedRewriteExclusives::default(),
            ),
            (
                r#"{"per_thread":{},"global":[],"rewrite_exclusives_in_memory":true}"#,
                RecordedRewriteExclusives {
                    rewrite: None,
                    in_memory: true,
                    stripped_heuristic: false,
                },
            ),
            (
                r#"{"per_thread":{},"global":[],"rewrite_exclusives_stripped_heuristic":true}"#,
                RecordedRewriteExclusives {
                    rewrite: None,
                    in_memory: false,
                    stripped_heuristic: true,
                },
            ),
        ] {
            std::fs::write(&path, json).unwrap();
            assert_eq!(
                read_recorded_rewrite_exclusives(&path),
                Ok(expected),
                "{json}"
            );
            let back: PreemptionRecord = serde_json::from_str(json).unwrap();
            assert_eq!(back.rewrite_exclusives(), None, "{json}");
            if !expected.stripped_heuristic {
                // A legacy key no record field owns is read for admission
                // only, and is not written back.
                assert_eq!(serde_json::to_string(&back).unwrap(), json, "{json}");
            }
        }
        let provenance = |mode, format_version| {
            Some(RewriteExclusives {
                mode,
                format_version,
            })
        };
        let stated = |rewrite, in_memory| RecordedRewriteExclusives {
            rewrite,
            in_memory,
            stripped_heuristic: false,
        };
        let heuristic = RecordedRewriteExclusives {
            rewrite: None,
            in_memory: false,
            stripped_heuristic: true,
        };
        for (recorded, replay, refused) in [
            (heuristic, None, true),
            (
                heuristic,
                provenance(RewriteExclusivesMode::Overlay, 6),
                true,
            ),
            (
                heuristic,
                provenance(RewriteExclusivesMode::Memory, 6),
                true,
            ),
            (stated(None, false), None, false),
            (
                stated(None, false),
                provenance(RewriteExclusivesMode::Overlay, 6),
                true,
            ),
            (
                stated(None, false),
                provenance(RewriteExclusivesMode::Memory, 6),
                true,
            ),
            (stated(None, true), None, true),
            // Same in-memory setting on both sides, but the record states
            // no format version.
            (
                stated(None, true),
                provenance(RewriteExclusivesMode::Memory, 6),
                true,
            ),
            (
                stated(provenance(RewriteExclusivesMode::Memory, 6), true),
                provenance(RewriteExclusivesMode::Memory, 6),
                false,
            ),
            (
                stated(provenance(RewriteExclusivesMode::Memory, 5), true),
                provenance(RewriteExclusivesMode::Memory, 6),
                true,
            ),
            (
                stated(provenance(RewriteExclusivesMode::Memory, 6), true),
                provenance(RewriteExclusivesMode::Overlay, 6),
                true,
            ),
            (
                stated(provenance(RewriteExclusivesMode::Overlay, 6), false),
                provenance(RewriteExclusivesMode::Overlay, 6),
                false,
            ),
            (
                stated(provenance(RewriteExclusivesMode::Overlay, 6), false),
                None,
                true,
            ),
            (
                stated(provenance(RewriteExclusivesMode::Overlay, 5), false),
                provenance(RewriteExclusivesMode::Overlay, 6),
                true,
            ),
        ] {
            assert_eq!(
                rewrite_exclusives_mismatch("--replay-schedule-from=x", recorded, replay).is_some(),
                refused,
                "{recorded:?} replayed with {replay:?}"
            );
        }
    }

    #[test]
    fn the_signal_state_copy_setting_round_trips_and_is_compared() {
        let unsaid = PreemptionRecord::from_sched_events(Vec::new());
        assert_eq!(unsaid.signal_state_copy(), None);
        assert!(
            !serde_json::to_string(&unsaid)
                .unwrap()
                .contains("signal_state_copy")
        );
        let dir = tempfile::tempdir().unwrap();
        for on in [true, false] {
            let written = PreemptionWriter::new(None)
                .with_signal_state_copy(on)
                .into_string();
            assert!(
                written.contains(&format!("\"signal_state_copy\":{on}")),
                "{written}"
            );
            let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(back.signal_state_copy(), Some(on));
            assert_eq!(back.clone_preemptions_only().signal_state_copy(), Some(on));
            let path = dir.path().join("record.json");
            std::fs::write(&path, &written).unwrap();
            assert_eq!(read_recorded_signal_state_copy(&path), Ok(Some(on)));
        }
        for (recorded, replay, refused) in [
            (None, true, false),
            (None, false, false),
            (Some(true), true, false),
            (Some(false), false, false),
            (Some(true), false, true),
            (Some(false), true, true),
        ] {
            assert_eq!(
                signal_state_copy_mismatch("--replay-schedule-from=x", recorded, replay).is_some(),
                refused,
                "{recorded:?} replayed with {replay}"
            );
        }
    }

    /// `--cap-preemption-charge` round-trips through the writer, the record
    /// and the settings reader; off writes the record as before; a replay
    /// under the other setting is refused.
    #[test]
    fn the_cap_preemption_charge_setting_round_trips_and_is_compared() {
        let off = PreemptionWriter::new(None).into_string();
        assert!(!off.contains("cap_preemption_charge"), "{off}");
        let written = PreemptionWriter::new(None)
            .with_cap_preemption_charge(true)
            .into_string();
        assert!(
            written.contains("\"cap_preemption_charge\":true"),
            "{written}"
        );
        let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
        assert!(back.cap_preemption_charge());
        assert!(back.clone_preemptions_only().cap_preemption_charge());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        std::fs::write(&path, &written).unwrap();
        assert_eq!(read_recorded_cap_preemption_charge(&path), Ok(true));
        std::fs::write(&path, &off).unwrap();
        assert_eq!(read_recorded_cap_preemption_charge(&path), Ok(false));
        for (recorded, replay, refused) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
            (false, true, true),
        ] {
            assert_eq!(
                cap_preemption_charge_mismatch("--replay-schedule-from=x", recorded, replay)
                    .is_some(),
                refused,
                "{recorded} replayed with {replay}"
            );
        }
    }

    /// The run's failed store-exclusives ride in a record so that a replay
    /// can label a divergence: written only when nonzero, so a record of a
    /// run without failures is byte-for-byte what it was before the field,
    /// and read as zero from records that lack it. The count is the host's,
    /// so it never makes two records unequal, and a copy of the preemptions
    /// alone does not carry it.
    #[test]
    fn store_exclusive_failures_round_trip_and_legacy_records_have_none() {
        let none = PreemptionWriter::new(None).with_signal_state_copy(true);
        let legacy = none.into_string();
        assert!(!legacy.contains("store_exclusive_failures"), "{legacy}");
        let back: PreemptionRecord = serde_json::from_str(&legacy).unwrap();
        assert_eq!(back.store_exclusive_failures(), 0);

        let mut zero = PreemptionWriter::new(None).with_signal_state_copy(true);
        zero.record_store_exclusive_failures(0);
        assert_eq!(zero.into_string(), legacy);

        let mut writer = PreemptionWriter::new(None).with_signal_state_copy(true);
        writer.record_store_exclusive_failures(20);
        let written = writer.into_string();
        assert!(
            written.contains("\"store_exclusive_failures\":20"),
            "{written}"
        );
        let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
        assert_eq!(back.store_exclusive_failures(), 20);
        assert_eq!(back.to_string(), written);
        assert_eq!(back.clone_preemptions_only().store_exclusive_failures(), 0);

        let without: PreemptionRecord = serde_json::from_str(&legacy).unwrap();
        assert_eq!(back, without);
        let hash = |record: &PreemptionRecord| {
            use std::hash::Hash;
            use std::hash::Hasher;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            record.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(&back), hash(&without));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn chaos_epoch_transitions_round_trip_and_replay_exact_factors() {
        let tid = DetTid::from_raw(2);
        let first = ChaosEpochTransition {
            logical_time: LogicalTime::from_nanos(100),
            epoch: 0,
            factor: RcbTimeMultiplier::from_f64(2.5),
        };
        let second = ChaosEpochTransition {
            logical_time: LogicalTime::from_nanos(500),
            epoch: 1,
            factor: RcbTimeMultiplier::from_f64(0.75),
        };

        let mut writer = PreemptionWriter::new(None);
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_chaos_epoch(tid, first);
        writer.insert_chaos_epoch(tid, second);
        let encoded = writer.into_string();
        assert!(encoded.contains("chaos_epochs"));

        let decoded: PreemptionRecord = serde_json::from_str(&encoded).unwrap();
        decoded.validate().unwrap();
        let mut history = decoded.extract_all().remove(&tid).unwrap().into_iter();
        assert_eq!(
            history.advance_chaos_epoch(LogicalTime::from_nanos(99)),
            None
        );
        assert_eq!(
            history.advance_chaos_epoch(LogicalTime::from_nanos(100)),
            Some(first)
        );
        assert_eq!(
            history.advance_chaos_epoch(LogicalTime::from_nanos(499)),
            None
        );
        assert_eq!(
            history.advance_chaos_epoch(LogicalTime::from_nanos(500)),
            Some(second)
        );
        assert_eq!(history.advance_chaos_epoch(LogicalTime::MAX), None);
    }

    /// Whether a Hermit that predates instruction offsets reads a record
    /// stating `format` (`None` for format 1): it reads formats 1 to the one
    /// below [`INSTRUCTION_OFFSET_ROWS_FORMAT`] (main's background starts, then
    /// the stopless slice ends this change is stacked on) and refuses an
    /// explicit 1, 0 and every later number by name (its `validate_format`).
    fn read_before_instruction_offsets(format: Option<u32>) -> bool {
        format.is_none_or(|format| {
            (TIME_SKIPS_FORMAT..INSTRUCTION_OFFSET_ROWS_FORMAT).contains(&format)
        })
    }

    /// The record the scheduler's writer leaves for a run that follows no
    /// record: one branch event, and two rows, the second with `offset`.
    fn written_run(offset: Option<InstructionOffset>) -> PreemptionRecord {
        let tid = DetTid::from_raw(3);
        let at = |ns| LogicalTime::from_nanos(ns);
        let mut writer =
            PreemptionWriter::new(None).with_format(Some(PREEMPTION_RECORD_FORMAT_MAX));
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_schedevent(SchedEvent::branches(tid, 20));
        writer.insert_reprioritization(tid, at(10), 1, DEFAULT_PRIORITY, 5, None);
        writer.insert_reprioritization(tid, at(20), 20, 5, 6, offset);
        let record: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
        record.validate().unwrap();
        record
    }

    const FOUR_STEPS: InstructionOffset = InstructionOffset {
        rcb_target: 18,
        instructions: 4,
    };

    /// A record whose rows hold an instruction offset states
    /// `INSTRUCTION_OFFSET_ROWS_FORMAT`, which a Hermit that predates the offsets
    /// refuses, never a format such a Hermit reads while it drops the
    /// offsets and stops each such slice at its branch: as a run's writer
    /// writes it, and as every copy of its rows alone (`clone_preemptions_only`,
    /// `preemptions_only`), every edit and every restated format keep it,
    /// through JSON. A record without an offset stays in a format such a
    /// Hermit reads. This Hermit refuses offset rows in any older format,
    /// through the loader a replay uses (`Recording::decode`), and still
    /// reads the same records without the offsets in every older format.
    #[test]
    fn instruction_offset_rows_state_a_format_older_hermits_refuse() {
        use crate::preemptions::boundary_recording::Recording;
        let plain = written_run(None);
        assert_eq!(plain.format_version(), Some(SWITCH_IN_CHARGE_FORMAT));
        assert!(read_before_instruction_offsets(plain.format_version()));
        assert_eq!(plain.clone_preemptions_only().format_version(), None);
        let offsets = written_run(Some(FOUR_STEPS));
        let json: serde_json::Value = serde_json::from_str(&offsets.to_string()).unwrap();
        assert_eq!(json["format_version"], INSTRUCTION_OFFSET_ROWS_FORMAT);

        let mut in_place = offsets.clone();
        in_place.preemptions_only();
        assert_eq!(in_place, offsets.clone_preemptions_only());
        for (what, record) in [
            ("the run's record", offsets.clone()),
            ("its rows alone", offsets.clone_preemptions_only()),
            ("its rows alone, in place", in_place),
            ("an edit", offsets.with_latest_preempt_removed()),
            ("the current format", offsets.clone().with_current_format()),
            (
                "format 3 restated",
                offsets.clone().with_format(Some(SWITCH_IN_CHARGE_FORMAT)),
            ),
            ("format 1 restated", offsets.clone().with_format(None)),
        ] {
            let back: PreemptionRecord = serde_json::from_str(&record.to_string()).unwrap();
            back.validate().unwrap();
            assert_eq!(
                back.format_version(),
                Some(INSTRUCTION_OFFSET_ROWS_FORMAT),
                "{what}"
            );
            assert!(
                !read_before_instruction_offsets(back.format_version()),
                "{what}"
            );
            Recording::decode(record.to_string().as_bytes()).unwrap();
        }

        for stated in [None, Some(TIME_SKIPS_FORMAT), Some(SWITCH_IN_CHARGE_FORMAT)] {
            // Offset rows stated in an older format, as a writer that
            // predates the fence would leave them: without the mark a later
            // format alone sets (`background_starts_unnoted`).
            let mut unfenced = offsets.clone();
            unfenced.format_version = stated;
            unfenced.background_starts_unnoted = false;
            let refusal = unfenced.validate_format().unwrap_err();
            assert!(
                refusal.contains("instruction offsets in its rows"),
                "{refusal}"
            );
            let decoded = Recording::decode(unfenced.to_string().as_bytes()).unwrap_err();
            assert!(decoded.is::<RecordFormatRefusal>(), "{decoded:#}");
            let mut legacy = plain.clone();
            legacy.format_version = stated;
            legacy.validate().unwrap();
            Recording::decode(legacy.to_string().as_bytes()).unwrap();
        }
    }

    /// An offset-free record is written byte for byte as before the
    /// offsets existed (the fence of
    /// [`PREEMPTION_RECORD_FORMAT_WITHOUT_OFFSETS`]), so every Hermit
    /// still reads it. The expected bytes are main's writer's, not this
    /// writer's: produced at main
    /// 19ab73ea596cd79515fa49ff237fcef6a96a6545 by the same construction
    /// below through main's `PreemptionWriter` (whose
    /// `insert_reprioritization` takes no offset), run as
    /// `cargo test -p hermit-detcore --lib <probe> -- --nocapture` in a
    /// detached worktree of that commit; the file on disk is those
    /// bytes plus the newline `PreemptionRecord::write_to_disk`
    /// appends. The comparison covers the whole file, so any field
    /// that moves in an offset-free record fails it.
    #[test]
    fn an_offset_free_record_is_written_exactly_as_before_the_offsets() {
        let tid = DetTid::from_raw(3);
        let mut writer = PreemptionWriter::new(None).noting_background_starts();
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_schedevent(SchedEvent::branches(tid, 100));
        writer.insert_background_start(tid, 1);
        writer.insert_schedevent(SchedEvent::branches(tid, 50).taken_without_stop());
        writer.insert_schedevent(SchedEvent::branches(tid, 7));
        writer.insert_reprioritization(
            tid,
            LogicalTime::from_nanos(10),
            10,
            DEFAULT_PRIORITY,
            5,
            None,
        );
        writer.insert_reprioritization(tid, LogicalTime::from_nanos(20), 20, 5, 6, None);
        let written = writer.into_string();
        let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
        back.validate().unwrap();
        assert_eq!(
            back.format_version(),
            Some(PREEMPTION_RECORD_FORMAT_WITHOUT_OFFSETS)
        );
        assert_eq!(
            written + "\n",
            concat!(
                r#"{"per_thread":{"3":{"final_prio":6,"prio_changes":[[10,1000],[20,5]],"preemption_rcbs":[10,20],"chaos_epochs":[]}},"#,
                r#""global":[{"dettid":3,"op":"Branch","count":100,"start_rip":null,"end_rip":null,"end_time":null},{"dettid":3,"op":"Branch","count":50,"start_rip":null,"end_rip":null,"end_time":null,"stopless":true},{"dettid":3,"op":"Branch","count":7,"start_rip":null,"end_rip":null,"end_time":null}],"#,
                r#""format_version":5,"rejoins":[],"background_starts":[{"after":1,"thread":3,"call":1}]}"#,
                "\n"
            )
        );
    }

    /// A run that notes its background starts: two events with one start
    /// between them, and two rows, each with an instruction offset when
    /// `offsets` says so.
    fn noting_run(offsets: bool) -> PreemptionRecord {
        let tid = DetTid::from_raw(3);
        let at = |ns| LogicalTime::from_nanos(ns);
        let two_steps = InstructionOffset {
            rcb_target: 8,
            instructions: 2,
        };
        let mut writer = PreemptionWriter::new(None).noting_background_starts();
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_schedevent(SchedEvent::branches(tid, 10));
        writer.insert_background_start(tid, 1);
        writer.insert_schedevent(SchedEvent::branches(tid, 20));
        writer.insert_reprioritization(
            tid,
            at(10),
            10,
            DEFAULT_PRIORITY,
            5,
            offsets.then_some(two_steps),
        );
        writer.insert_reprioritization(tid, at(20), 20, 5, 6, offsets.then_some(FOUR_STEPS));
        let record: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
        record.validate().unwrap();
        assert_eq!(record.background_starts().len(), 1);
        assert!(record.notes_background_starts());
        record
    }

    /// A record that drops its background starts while its rows keep an
    /// instruction offset keeps `INSTRUCTION_OFFSET_ROWS_FORMAT`, which a
    /// Hermit that predates the offsets must go on refusing, and says by its
    /// mark (`background_starts_unnoted`) that it notes no start, where the
    /// number alone would claim them all: after every transform that drops
    /// the starts (a shorter schedule prefix, an edit, a writer that notes
    /// none, a restated current format) and through JSON. The same
    /// transforms of a record without offsets lower it below
    /// `BACKGROUND_STARTS_FORMAT`, as main does, and write no mark: that is
    /// the control. A whole prefix keeps the starts and the claim; rows alone
    /// note none by their own mark (`rows_alone`); a mixed order keeps no
    /// rows, so no offsets, and goes below the starts. Validation refuses the
    /// mark beside a list of starts and below `INSTRUCTION_OFFSET_ROWS_FORMAT`.
    /// With the mark ignored by `notes_background_starts`, the shorter prefix,
    /// the edit and the writer each claim starts they do not hold.
    /// A record that notes its background starts, marks a lone thread's
    /// timeslice end taken without a stop (`SchedEvent::stopless`) and holds
    /// an instruction offset in its rows. Each transform that drops the
    /// starts and keeps the rows (a shorter prefix, an edit, a shared trial
    /// format) clears the `stopless` marks, which only a record that notes
    /// its starts holds, keeps `INSTRUCTION_OFFSET_ROWS_FORMAT` with the
    /// offsets and says it notes no starts, through JSON; a mixed order keeps
    /// no rows and goes below the starts, without marks. A writer that notes
    /// no starts, raised by its offset row, records the stopless end as an
    /// ordinary one. The whole prefix keeps the marks, the starts and the
    /// claim, and validation refuses a mark beside the flag. With the flag
    /// left out of `forget_stopless_marks_below_their_format`, the transforms
    /// keep the marks and this test fails.
    #[test]
    fn a_record_that_drops_its_starts_drops_its_stopless_marks_and_keeps_its_offsets() {
        let tid = DetTid::from_raw(3);
        let offset = InstructionOffset {
            rcb_target: 8,
            instructions: 2,
        };
        let written = |noting: bool| {
            let mut writer = PreemptionWriter::new(None);
            if noting {
                writer = writer.noting_background_starts();
            }
            writer.register_thread(tid, DEFAULT_PRIORITY);
            writer.insert_reprioritization(
                tid,
                LogicalTime::from_nanos(10),
                10,
                DEFAULT_PRIORITY,
                5,
                Some(offset),
            );
            writer.insert_schedevent(SchedEvent::branches(tid, 10).taken_without_stop());
            writer.insert_background_start(tid, 1);
            writer.insert_schedevent(SchedEvent::branches(tid, 20));
            let record: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
            record.validate().unwrap();
            record
        };
        let marks =
            |record: &PreemptionRecord| record.global.iter().filter(|event| event.stopless).count();
        let back = |record: PreemptionRecord, what: &str| {
            let back: PreemptionRecord = serde_json::from_str(&record.to_string()).unwrap();
            back.validate()
                .unwrap_or_else(|error| panic!("{what}: {error}"));
            back
        };
        let run = written(true);
        assert_eq!(run.format_version(), Some(INSTRUCTION_OFFSET_ROWS_FORMAT));
        assert!(run.notes_background_starts());
        assert_eq!((marks(&run), run.background_starts().len()), (1, 1));

        let whole = back(run.schedule_prefix(2), "the whole prefix");
        assert_eq!(
            (
                whole.format_version(),
                marks(&whole),
                whole.background_starts().len(),
                whole.background_starts_unnoted
            ),
            (Some(INSTRUCTION_OFFSET_ROWS_FORMAT), 1, 1, false)
        );
        let mut edited = run.clone();
        edited.schedevents_iter_mut().for_each(|_| ());
        // An analyze or bisect trial is built from a schedule, which holds no
        // starts, and states the sources' shared format below them.
        let mut schedule = run.clone();
        schedule.background_starts.clear();
        let trial = schedule.with_format(format_without_background_starts(
            format_without_instruction_offsets(run.format_version()),
        ));
        for (what, record) in [
            ("a shorter prefix", run.schedule_prefix(1)),
            ("an edit", edited),
            ("a shared trial format", trial),
        ] {
            let record = back(record, what);
            assert_eq!(
                record.format_version(),
                Some(INSTRUCTION_OFFSET_ROWS_FORMAT),
                "{what}"
            );
            assert!(record.rows_carry_instruction_offsets(), "{what}");
            assert!(record.background_starts_unnoted, "{what}");
            assert!(!record.notes_background_starts(), "{what}");
            assert!(record.background_starts().is_empty(), "{what}");
            assert_eq!(marks(&record), 0, "{what}");
        }
        let mixed = back(
            run.interpolate_events(&run, run.schedevents().iter().rev().cloned().collect())
                .unwrap(),
            "a mixed order",
        );
        assert_eq!(
            (mixed.format_version(), marks(&mixed)),
            (Some(BACKGROUND_STARTS_FORMAT - 1), 0)
        );

        let plain = written(false);
        assert_eq!(plain.format_version(), Some(INSTRUCTION_OFFSET_ROWS_FORMAT));
        assert!(plain.background_starts_unnoted);
        assert_eq!((marks(&plain), plain.background_starts().len()), (0, 0));

        let mut forged = run.schedule_prefix(1);
        forged.global[0].stopless = true;
        let refusal = forged.validate().unwrap_err();
        assert!(
            refusal.contains("background_starts_unnoted") && refusal.contains("stopless"),
            "{refusal}"
        );
    }

    /// The combined lowering: a `stopless` end that only its mark keeps
    /// visible (a marked Branch the guest reached a same-thread check-in
    /// from, as in `a_lowering_that_would_hide_a_stopless_end_is_refused`)
    /// in a record whose rows hold an instruction offset. Dropping its
    /// background starts keeps `INSTRUCTION_OFFSET_ROWS_FORMAT` with the
    /// flag, at or above `STOPLESS_SLICE_ENDS_FORMAT`, so the format alone
    /// no longer refuses the kept mark: the conversion must still refuse,
    /// through the flag, rather than clear the mark and silently remove
    /// the slice end (its grant and virtual-time charge). Clearing every
    /// mark under the flag, visible or not, makes each refusal below
    /// validate instead and this test fails. The same end followed by a
    /// Branch stays visible without its mark and still lowers.
    #[test]
    fn a_record_that_drops_its_starts_refuses_an_invisible_stopless_end() {
        let tid = DetTid::from_raw(3);
        let offset = InstructionOffset {
            rcb_target: 8,
            instructions: 2,
        };
        let check_in = SchedEvent {
            dettid: tid,
            op: Op::OtherInstructions,
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: None,
            stopless: false,
        };
        let marks =
            |record: &PreemptionRecord| record.global.iter().filter(|event| event.stopless).count();
        let writer_with = |noting: bool, invisible: bool| {
            let mut writer = PreemptionWriter::new(None);
            if noting {
                writer = writer.noting_background_starts();
            }
            writer.register_thread(tid, DEFAULT_PRIORITY);
            writer.insert_reprioritization(
                tid,
                LogicalTime::from_nanos(10),
                10,
                DEFAULT_PRIORITY,
                5,
                Some(offset),
            );
            writer.insert_schedevent(SchedEvent::branches(tid, 10).taken_without_stop());
            writer.insert_background_start(tid, 1);
            if invisible {
                writer.insert_schedevent(check_in.clone());
            }
            writer.insert_schedevent(SchedEvent::branches(tid, 20));
            writer
        };

        let run: PreemptionRecord =
            serde_json::from_str(&writer_with(true, true).into_string()).unwrap();
        run.validate().unwrap();
        assert_eq!(run.format_version(), Some(INSTRUCTION_OFFSET_ROWS_FORMAT));
        assert!(run.notes_background_starts());
        assert_eq!((marks(&run), run.background_starts().len()), (1, 1));
        // The whole prefix keeps the starts, the mark and the claim.
        run.schedule_prefix(3).validate().unwrap();
        // Each transform that drops the starts and keeps the rows keeps
        // the invisible mark too, and validation refuses it by the flag.
        let mut edited = run.clone();
        edited.schedevents_iter_mut().for_each(|_| ());
        for (what, record) in [
            ("a shorter prefix", run.schedule_prefix(2)),
            ("an edit", edited),
        ] {
            assert_eq!(
                record.format_version(),
                Some(INSTRUCTION_OFFSET_ROWS_FORMAT),
                "{what}"
            );
            assert!(record.background_starts_unnoted, "{what}");
            assert_eq!(marks(&record), 1, "{what}: the mark is kept");
            let refusal = record.validate().unwrap_err();
            assert!(
                refusal.contains("background_starts_unnoted") && refusal.contains("stopless"),
                "{what}: {refusal}"
            );
        }
        // A writer that notes no starts, raised by its offset row,
        // recording the same sequence is refused at flush.
        let refusal = writer_with(false, true)
            .serialized_string()
            .expect_err("a writer that would hide the end must refuse");
        assert!(
            refusal.contains("background_starts_unnoted") && refusal.contains("stopless"),
            "{refusal}"
        );

        // Control: the same end followed by a Branch stays visible
        // without its mark, so dropping the starts clears it and the
        // record still validates, at the offsets' format with the flag.
        let visible: PreemptionRecord =
            serde_json::from_str(&writer_with(true, false).into_string()).unwrap();
        let lowered = visible.schedule_prefix(1);
        lowered.validate().unwrap();
        assert!(lowered.background_starts_unnoted);
        assert_eq!(marks(&lowered), 0);
        let plain: PreemptionRecord =
            serde_json::from_str(&writer_with(false, false).into_string()).unwrap();
        plain.validate().unwrap();
        assert!(plain.background_starts_unnoted);
        assert_eq!(marks(&plain), 0);
    }

    #[test]
    fn a_record_that_drops_its_background_starts_keeps_its_offsets_format_and_says_so() {
        let below = format_without_background_starts(Some(BACKGROUND_STARTS_FORMAT));
        let check = |what: &str,
                     record: &PreemptionRecord,
                     format: Option<u32>,
                     notes: bool,
                     marked: bool| {
            let json = record.to_string();
            let back: PreemptionRecord = serde_json::from_str(&json).unwrap();
            back.validate()
                .unwrap_or_else(|error| panic!("{what}: {error}"));
            assert_eq!(back, *record, "{what}");
            assert_eq!(back.format_version(), format, "{what}");
            assert_eq!(back.notes_background_starts(), notes, "{what}");
            assert_eq!(back.background_starts_unnoted, marked, "{what}");
            assert_eq!(
                json.contains("background_starts_unnoted"),
                marked,
                "{what}: {json}"
            );
            if !notes {
                assert!(back.background_starts().is_empty(), "{what}");
            }
        };
        for offsets in [true, false] {
            let run = noting_run(offsets);
            let (kept, dropped, marked) = if offsets {
                (
                    Some(INSTRUCTION_OFFSET_ROWS_FORMAT),
                    Some(INSTRUCTION_OFFSET_ROWS_FORMAT),
                    true,
                )
            } else {
                (Some(STOPLESS_SLICE_ENDS_FORMAT), below, false)
            };
            let case = |what: &str| format!("{what}, offsets {offsets}");
            check(&case("the run's record"), &run, kept, true, false);
            check(
                &case("its whole prefix"),
                &run.schedule_prefix(run.schedevents().len()),
                kept,
                true,
                false,
            );
            for len in [0, 1] {
                check(
                    &case(&format!("a prefix of {len} events")),
                    &run.schedule_prefix(len),
                    dropped,
                    false,
                    marked,
                );
            }
            check(
                &case("an edit"),
                &run.with_latest_preempt_removed(),
                dropped,
                false,
                marked,
            );
            check(
                &case("the current format restated"),
                &run.schedule_prefix(1).with_current_format(),
                dropped,
                false,
                marked,
            );
            check(
                &case("the run's current format restated"),
                &run.clone().with_current_format(),
                kept,
                true,
                false,
            );
            let rows = run.clone_preemptions_only();
            check(
                &case("its rows alone"),
                &rows,
                offsets.then_some(INSTRUCTION_OFFSET_ROWS_FORMAT),
                false,
                false,
            );
            let mut in_place = run.clone();
            in_place.preemptions_only();
            if offsets {
                assert_eq!(in_place, rows);
            }
            check(
                &case("its rows alone, in place"),
                &in_place,
                if offsets { kept } else { below },
                false,
                false,
            );
            let mixed = run
                .interpolate_events(&run, run.schedevents().iter().rev().cloned().collect())
                .unwrap();
            check(&case("a mixed order"), &mixed, below, false, false);

            // A writer that notes no starts: an offset row raises it to the
            // offsets' format, still noting none, so it takes no start.
            let tid = DetTid::from_raw(3);
            for format in [None, Some(PREEMPTION_RECORD_FORMAT_MAX)] {
                let mut writer = PreemptionWriter::new(None).with_format(format);
                writer.register_thread(tid, DEFAULT_PRIORITY);
                writer.insert_schedevent(SchedEvent::branches(tid, 20));
                writer.insert_reprioritization(
                    tid,
                    LogicalTime::from_nanos(20),
                    20,
                    DEFAULT_PRIORITY,
                    5,
                    offsets.then_some(FOUR_STEPS),
                );
                writer.insert_background_start(tid, 1);
                let written: PreemptionRecord =
                    serde_json::from_str(&writer.into_string()).unwrap();
                let plain = format_without_background_starts(format);
                check(
                    &case(&format!("a writer that notes no starts, from {format:?}")),
                    &written,
                    if offsets {
                        Some(INSTRUCTION_OFFSET_ROWS_FORMAT)
                    } else {
                        plain
                    },
                    false,
                    offsets,
                );
            }
        }

        let mut marked = noting_run(true).schedule_prefix(1);
        assert!(marked.background_starts_unnoted);
        let mut listed = marked.clone();
        listed.background_starts = noting_run(true).background_starts().to_vec();
        let refusal = listed.validate().unwrap_err();
        assert!(refusal.contains("background_starts_unnoted"), "{refusal}");
        for stated in [
            Some(STOPLESS_SLICE_ENDS_FORMAT),
            Some(BACKGROUND_STARTS_FORMAT),
            below,
            None,
        ] {
            marked.format_version = stated;
            let refusal = marked.validate_contents().unwrap_err();
            assert!(
                refusal.contains("background_starts_unnoted")
                    && refusal.contains("below the format"),
                "{stated:?}: {refusal}"
            );
        }
        // A format this Hermit does not read is refused by name, whatever
        // the record holds, the mark included (`validate_format`).
        for unread in [0, 1, PREEMPTION_RECORD_FORMAT_MAX + 1] {
            marked.format_version = Some(unread);
            marked.validate_contents().unwrap();
            let refusal = marked.validate().unwrap_err();
            assert!(
                refusal.contains("this Hermit reads formats 1 to"),
                "format {unread}: {refusal}"
            );
        }
    }

    /// Rows alone in `INSTRUCTION_OFFSET_ROWS_FORMAT` follow format 1's timing
    /// rules, as the same rows without an offset state no format
    /// (`timing_format`): no reader that keys timing on the format takes
    /// their format 6 for a claim to list every jump of global time or to
    /// charge replayed switch-ins, which index or replay a schedule trace
    /// they do not hold. Here they meet each such reader in this file: the
    /// timing predicates, the loader a replay uses, `validate_format`'s rule
    /// that jumps need a listing format, `with_time_skips` and the writer's
    /// jump, which never lower the format, and interpolation, which refuses
    /// to mix them with a schedule stating the same number. The scheduler's
    /// readers are `a_replay_charges_switch_ins_by_its_records_format`'s, and
    /// analyze's `time_skip_carry`'s own test.
    #[test]
    fn rows_alone_with_instruction_offsets_follow_format_one_timing() {
        use crate::preemptions::boundary_recording::Recording;
        let tid = DetTid::from_raw(3);
        let at = |ns| LogicalTime::from_nanos(ns);
        let schedule = written_run(Some(FOUR_STEPS));
        assert_eq!(
            schedule.timing_format(),
            Some(INSTRUCTION_OFFSET_ROWS_FORMAT)
        );
        assert!(schedule.lists_its_time_skips());
        assert!(schedule.charges_replayed_switch_ins());

        let timing = |record: &PreemptionRecord| {
            (
                record.timing_format(),
                record.lists_its_time_skips(),
                record.charges_replayed_switch_ins(),
            )
        };
        let rows = schedule.clone_preemptions_only();
        assert_eq!(rows.format_version(), Some(INSTRUCTION_OFFSET_ROWS_FORMAT));
        assert_eq!(timing(&rows), (None, false, false));
        assert_eq!(
            timing(&rows),
            timing(&written_run(None).clone_preemptions_only())
        );
        let loaded = Recording::decode(rows.to_string().as_bytes()).unwrap();
        assert_eq!(
            loaded.format_version(),
            Some(INSTRUCTION_OFFSET_ROWS_FORMAT)
        );
        assert_eq!(
            (
                loaded.timing_format(),
                loaded.lists_its_time_skips(),
                loaded.charges_replayed_switch_ins()
            ),
            (None, false, false)
        );

        let jump = TimeSkip {
            event: 0,
            target: at(1_000),
            reason: TimeSkipReason::IdleGuestPollers,
        };
        let jumped = rows.clone().with_time_skips(vec![jump]);
        assert_eq!(
            jumped.format_version(),
            Some(INSTRUCTION_OFFSET_ROWS_FORMAT)
        );
        assert!(jumped.validate_format().unwrap_err().contains("time skips"));
        schedule
            .clone()
            .with_time_skips(vec![jump])
            .validate()
            .unwrap();
        // A jump the writer records before its first event, while it holds
        // rows alone, keeps the format of its offset row.
        let mut writer =
            PreemptionWriter::new(None).with_format(Some(PREEMPTION_RECORD_FORMAT_MAX));
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_reprioritization(tid, at(10), 18, DEFAULT_PRIORITY, 5, Some(FOUR_STEPS));
        writer.insert_time_skip(jump);
        writer.insert_schedevent(SchedEvent::branches(tid, 20));
        let written: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
        assert_eq!(
            written.format_version(),
            Some(INSTRUCTION_OFFSET_ROWS_FORMAT)
        );
        assert!(written.lists_its_time_skips());
        written.validate().unwrap();

        let refusal = rows
            .interpolate_events(&schedule, schedule.schedevents().clone())
            .unwrap_err();
        assert!(refusal.contains("replay rules differ"), "{refusal}");

        // The mark, not an empty schedule trace, makes rows alone: a prefix
        // of no events keeps its format's timing, and rows alone that hold
        // a schedule trace are malformed.
        let empty = schedule.schedule_prefix(0);
        assert_eq!(empty.timing_format(), Some(INSTRUCTION_OFFSET_ROWS_FORMAT));
        let mut traced = rows.clone();
        traced.global = schedule.schedevents().clone();
        assert!(
            traced
                .validate_contents()
                .unwrap_err()
                .contains("rows alone")
        );
    }

    /// The instruction offsets ride in a thread's history beside its RCB
    /// targets: empty until a timeslice has one (so a record written without
    /// `--chaos-instruction-offset` is the same as before), then one entry per
    /// row, kept in step when a row is dropped or the contents stripped,
    /// through JSON, and checked by `validate`.
    #[test]
    fn instruction_offsets_ride_beside_their_rows() {
        let tid = DetTid::from_raw(3);
        let at = |ns| LogicalTime::from_nanos(ns);
        let mut off = PreemptionWriter::new(None);
        off.register_thread(tid, DEFAULT_PRIORITY);
        off.insert_reprioritization(tid, at(10), 1, DEFAULT_PRIORITY, 5, None);
        let off = off.into_string();
        assert!(!off.contains("preemption_instruction_offsets"), "{off}");

        let offset = InstructionOffset {
            rcb_target: 18,
            instructions: 4,
        };
        let mut writer = PreemptionWriter::new(None);
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_reprioritization(tid, at(10), 1, DEFAULT_PRIORITY, 5, None);
        writer.insert_reprioritization(tid, at(20), 20, 5, 6, Some(offset));
        writer.insert_reprioritization(tid, at(30), 30, 6, 7, None);
        let record: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
        record.validate().unwrap();
        let history = &record.per_thread[&tid];
        assert_eq!(
            history.preemption_instruction_offsets,
            vec![None, Some(offset), None]
        );
        assert!(history.has_instruction_offsets());
        // The RPC's encoding keeps every field, empty or not.
        for history in [history.clone(), ThreadHistory::new()] {
            let config = bincode::config::standard();
            let encoded = bincode::serde::encode_to_vec(&history, config).unwrap();
            let (decoded, _): (ThreadHistory, usize) =
                bincode::serde::decode_from_slice(&encoded, config).unwrap();
            assert_eq!(decoded, history);
        }
        let mut rows = history.clone().into_iter();
        assert_eq!(
            rows.next_with_offset(),
            Some((at(10), DEFAULT_PRIORITY, Some(1), None))
        );
        assert_eq!(
            rows.next_with_offset(),
            Some((at(20), 5, Some(20), Some(offset)))
        );
        assert_eq!(rows.next_with_rcbs(), Some((at(30), 6, Some(30))));

        let dropped = record.with_latest_preempt_removed();
        dropped.validate().unwrap();
        assert_eq!(
            dropped.per_thread[&tid].preemption_instruction_offsets,
            vec![None, Some(offset)]
        );
        let stripped = record.clone().strip_contents();
        assert!(!stripped.per_thread[&tid].has_instruction_offsets());
        stripped.validate().unwrap();

        let mut short = record.clone();
        short
            .per_thread
            .get_mut(&tid)
            .unwrap()
            .preemption_instruction_offsets
            .pop();
        assert!(
            short
                .validate()
                .unwrap_err()
                .contains("instruction offsets")
        );
        let mut zero = record.clone();
        zero.per_thread
            .get_mut(&tid)
            .unwrap()
            .preemption_instruction_offsets[1] = Some(InstructionOffset {
            rcb_target: 18,
            instructions: 0,
        });
        assert!(zero.validate().unwrap_err().contains("zero steps"));
        // The row ends at branch 20: an offset aimed past it cannot be its.
        let mut early = record.clone();
        early
            .per_thread
            .get_mut(&tid)
            .unwrap()
            .preemption_instruction_offsets[1] = Some(InstructionOffset {
            rcb_target: 21,
            instructions: 4,
        });
        assert!(early.validate().unwrap_err().contains("precedes"));
        let mut without_rcbs = record.clone();
        without_rcbs
            .per_thread
            .get_mut(&tid)
            .unwrap()
            .preemption_rcbs
            .clear();
        assert!(
            without_rcbs
                .validate()
                .unwrap_err()
                .contains("no RCB targets")
        );

        let dir = tempfile::tempdir().unwrap();
        let (with, without) = (
            dir.path().join("with.json"),
            dir.path().join("without.json"),
        );
        std::fs::write(&with, record.to_string()).unwrap();
        std::fs::write(&without, &off).unwrap();
        assert_eq!(read_recorded_instruction_offsets(&with), Ok(true));
        assert_eq!(read_recorded_instruction_offsets(&without), Ok(false));
    }

    /// <https://github.com/rrnewton/hermit/issues/3411>: the epoch a record was
    /// made under survives serialization, the preemptions-only copy, and the
    /// non-panicking CLI reader; records written before it was stored read as
    /// `None`, and a malformed file is an error rather than a panic.
    #[test]
    fn recorded_epoch_round_trips_and_legacy_records_have_none() {
        let epoch: DateTime<Utc> = "2000-12-31T23:59:59.123456789Z".parse().unwrap();
        let tid = DetTid::from_raw(3);
        let mut writer = PreemptionWriter::new(None).with_epoch(epoch);
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_reprioritization(
            tid,
            LogicalTime::from_nanos(978_307_199_223_456_789),
            7,
            DEFAULT_PRIORITY,
            5,
            None,
        );
        let encoded = writer.into_string();

        let decoded: PreemptionRecord = serde_json::from_str(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.epoch(), Some(epoch));
        assert_eq!(decoded.clone_preemptions_only().epoch(), Some(epoch));

        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("current.json");
        std::fs::write(&current, &encoded).unwrap();
        assert_eq!(read_recorded_epoch(&current), Ok(Some(epoch)));

        let mut legacy_value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        legacy_value
            .as_object_mut()
            .unwrap()
            .remove("epoch")
            .unwrap();
        let legacy = directory.path().join("legacy.json");
        std::fs::write(&legacy, legacy_value.to_string()).unwrap();
        assert_eq!(read_recorded_epoch(&legacy), Ok(None));
        let legacy_record: PreemptionRecord = serde_json::from_value(legacy_value.clone()).unwrap();
        assert_eq!(legacy_record.epoch(), None);
        assert!(
            !serde_json::to_string(&legacy_record)
                .unwrap()
                .contains("\"epoch\"")
        );

        let malformed = directory.path().join("malformed.json");
        std::fs::write(&malformed, "{\"epoch\": 7}").unwrap();
        assert!(read_recorded_epoch(&malformed).is_err());
        assert!(read_recorded_epoch(&directory.path().join("absent.json")).is_err());
    }

    #[test]
    fn print_preemptionrecord() {
        let (file, path) = tempfile::NamedTempFile::new().unwrap().keep().unwrap();
        drop(file);
        let mut pw = PreemptionWriter::new(Some(path.clone()));
        let tid1 = DetTid::from_raw(2);
        let tid2 = DetTid::from_raw(4);
        pw.register_thread(tid1, 1000);
        pw.register_thread(tid2, 1000);

        pw.insert_reprioritization(tid1, LogicalTime::from_nanos(3), 3, 1000, 3, None);
        pw.insert_reprioritization(tid1, LogicalTime::from_nanos(30), 30, 3, 30, None);
        pw.insert_reprioritization(tid1, LogicalTime::from_nanos(300), 300, 30, 300, None);
        pw.insert_reprioritization(tid2, LogicalTime::from_nanos(2), 2, 1000, 2, None);
        pw.insert_reprioritization(tid2, LogicalTime::from_nanos(20), 20, 2, 20, None);
        pw.insert_reprioritization(tid2, LogicalTime::from_nanos(200), 200, 20, 200, None);

        // First, round trip through a pretty-printed string
        // ------------------------------------------------
        let str: String = serde_json::to_string_pretty(&pw.inner).unwrap();
        eprintln!("{}", str);
        let pr2: PreemptionRecord = serde_json::from_str(&str).unwrap();
        eprintln!("Round trip {:?}", pr2);
        assert_eq!(pw.inner, pr2);

        // Second, actually write it out to disk
        // -------------------------------------
        pw.flush().unwrap();
        let reader = PreemptionReader::new(&path);
        let th1 = reader.extract_thread_record(&tid1).unwrap();
        let th2 = reader.extract_thread_record(&tid2).unwrap();
        assert_eq!(th1.final_prio, 300);
        assert_eq!(th2.final_prio, 200);

        let mut exact = th1.clone().into_iter();
        assert_eq!(
            exact.next_with_rcbs(),
            Some((LogicalTime::from_nanos(3), 1000, Some(3)))
        );
        assert_eq!(
            exact.next_with_rcbs(),
            Some((LogicalTime::from_nanos(30), 3, Some(30)))
        );

        let it1 = th1.into_iter();
        let it2 = th2.into_iter();

        assert_eq!(it1.initial_priority(), 1000);
        assert_eq!(it2.initial_priority(), 1000);

        let v1: Vec<(LogicalTime, Priority)> = it1.collect();
        let v2: Vec<(LogicalTime, Priority)> = it2.collect();
        assert_eq!(
            v1,
            vec![
                (LogicalTime::from_nanos(3), 1000),
                (LogicalTime::from_nanos(30), 3),
                (LogicalTime::from_nanos(300), 30)
            ]
        );
        assert_eq!(
            v2,
            vec![
                (LogicalTime::from_nanos(2), 1000),
                (LogicalTime::from_nanos(20), 2),
                (LogicalTime::from_nanos(200), 20)
            ]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn round_trip_vec_representations() {
        let str = r#"{"per_thread":{"2":{"final_prio":1716,"prio_changes":[[946684799000013020,7301],[946684799000034020,9081],[946684799000041600,9238],[946684799000054790,865],
[946684799000057440,751],[946684799000061970,275],[946684799000062730,5135],[946684799000069530,6339],
[946684799000082850,1123],[946684799000101140,7875],[946684799000140625,4203],[946684799000171780,8611],
[946684799000183550,6306],[946684799000184440,7958],[946684799000195750,8919],[946684799000226150,69],
[946684799000236380,5915],[946684799000278180,3514],[946684799000320050,30],[946684799000334630,4629],
[946684799000344650,2926],[946684799000355020,710],[946684799000365030,3513],[946684799000386350,4881],
[946684799000396360,4852],[946684799000406840,4935],[946684799000426980,6672],[946684799000437970,7727],
[946684799000452410,7017],[946684799000462430,1572],[946684799000546210,6395],[946684799000548120,3726],
[946684799000562700,846],[946684799000583090,7838],[946684799000603310,8291],[946684799000655180,210],
[946684799000666230,4576],[946684799000680910,4974],[946684799000723020,9160],[946684799000776780,439],
[946684799000777080,6791],[946684799000787220,3015],[946684799000809090,7489],[946684799000840870,7165],
[946684799000852855,4326],[946684799000854355,358],[946684799000866575,4448],[946684799000903415,6848],
[946684799000913455,1899],[946684799000923630,2117],[946684799000963980,7705],[946684799001007090,8683],
[946684799001016950,3317],[946684799001017980,5261],[946684799001027540,3478],[946684799001029990,6474],
[946684799001053545,4823],[946684799001068395,4508],[946684799001073095,194],[946684799001121035,5944],
[946684799001171285,8408],[946684799001171295,4493],[946684799001192845,3481]]}},"global":[]}"#;
        let pr: PreemptionRecord = serde_json::from_str(str).unwrap();
        let vecs = pr.as_vecs();
        let pr2 = PreemptionRecord::from_vecs(&vecs);
        assert_eq!(pr, pr2);
    }

    /// A record with more distinct priorities than the renumbering range
    /// holds keeps its own instead of panicking (a long chaos run of six
    /// threads reached 9978 distinct priorities). One that fits is still
    /// renumbered, up to `LAST_PRIORITY`.
    #[test]
    fn a_record_too_varied_to_renumber_keeps_its_priorities() {
        let record_of = |count: Priority| {
            let changes: Vec<(LogicalTime, Priority)> = (FIRST_PRIORITY..FIRST_PRIORITY + count)
                .enumerate()
                .map(|(index, prio)| (LogicalTime::from_nanos(index as u64), prio))
                .collect();
            PreemptionRecord::from_vecs(&BTreeMap::from([(DetTid::from_raw(3), changes)]))
        };
        let slots = LAST_PRIORITY - DEFAULT_PRIORITY + 1;
        let fits = record_of(slots).normalize();
        fits.validate().unwrap();
        let highest = fits
            .as_vecs()
            .values()
            .flat_map(|history| history.iter().map(|(_, prio)| *prio))
            .max();
        assert_eq!(highest, Some(LAST_PRIORITY));
        let too_many = record_of(slots + 1);
        let kept = too_many.normalize();
        kept.validate().unwrap();
        assert_eq!(kept, too_many.without_default_threads());
    }

    #[test]
    fn normalize_preemption_record() {
        let str = r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"7":{"final_prio":9722,"prio_changes":[]},
"9":{"final_prio":9982,"prio_changes":[[946684799006227400,7839]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#;
        let pr: PreemptionRecord = serde_json::from_str(str).unwrap();
        let pr2 = pr.normalize();
        pr2.validate().unwrap();
        let bmap = pr2.as_vecs();
        // Normalization may remove the useless default-prio entry:
        if let Some(x) = bmap.get(&DetTid::from_raw(3)) {
            assert_eq!(x, &vec![(LogicalTime::from_nanos(0), 1000)]);
        }
        if let Some(x) = bmap.get(&DetTid::from_raw(5)) {
            assert_eq!(x, &vec![(LogicalTime::from_nanos(0), 1000)]);
        }
        assert_eq!(
            bmap.get(&DetTid::from_raw(7)).unwrap(),
            &vec![(LogicalTime::from_nanos(0), 1002)]
        );
        assert_eq!(
            bmap.get(&DetTid::from_raw(9)).unwrap(),
            &vec![
                (LogicalTime::from_nanos(0), 1001),
                (LogicalTime::from_nanos(946684799006227400), 1003)
            ]
        );
        if let Some(x) = bmap.get(&DetTid::from_raw(11)) {
            assert_eq!(x, &vec![(LogicalTime::from_nanos(0), 1000)]);
        }
    }

    #[test_case(
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1002,"prio_changes":[[946684799006227410,1000],[946684799006227415,1001]]},
"9":{"final_prio":1002,"prio_changes":[[946684799006227400,1000],[946684799006227405,1001]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#,
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1001,"prio_changes":[[946684799006227410,1000]]},
"9":{"final_prio":1002,"prio_changes":[[946684799006227400,1000],[946684799006227405,1001]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#
        ; "removes latest priority change and coalesces into final priority"
    )]
    #[test_case(
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1001,"prio_changes":[[946684799006227400,1000]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#,
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1000,"prio_changes":[]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#
        ; "removes only priority change and coalesces into final priority"
    )]
    #[test_case(
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1000,"prio_changes":[]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#,
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#
        ; "removes entire history for last tid if no non-default priority changes"
    )]
    fn with_latest_preempt_removed(pr_json: &str, expected_pr_json: &str) {
        let pr: PreemptionRecord = serde_json::from_str(pr_json).unwrap();
        let expected_pr: PreemptionRecord = serde_json::from_str(expected_pr_json).unwrap();

        let pr_with_latest_removed = pr.with_latest_preempt_removed();

        pr_with_latest_removed.validate().unwrap();
        self::assert_eq!(pr_with_latest_removed, expected_pr);
    }

    /// Prefix trials must keep the recording's replay settings and host
    /// inputs even when none, or only some, of its events are replayed.
    #[test]
    fn a_schedule_prefix_keeps_the_recorded_replay_settings_and_host_inputs() {
        use detcore_model::host_input::HostFileIdentity;
        use detcore_model::host_input::HostInput;

        let tid = DetTid::from_raw(3);
        let mut record = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(tid, 10),
            SchedEvent::branches(tid, 20),
        ])
        .with_cap_preemption_charge(true)
        .with_expired_wait_runs_charged_once(true);
        record.rng_seed = Some(1234567);
        record.host_inputs = Some(HostInputSet {
            digest: "ab".repeat(32),
            files: vec![HostInput {
                path: "/recorded/input".to_owned(),
                identity: HostFileIdentity {
                    dev: 2,
                    ino: 3,
                    size: 4,
                    mtime_sec: 5,
                    mtime_nsec: 6,
                    content: None,
                },
                content: Some("cd".repeat(32)),
            }],
            stdio: None,
        });
        for len in [0, 1, 2, 9] {
            let prefix = record.schedule_prefix(len);
            assert_eq!(prefix.global, record.global[..len.min(record.global.len())]);
            assert_eq!(
                (
                    prefix.cap_preemption_charge,
                    prefix.rng_seed,
                    prefix.expired_wait_runs_charged_once,
                    &prefix.host_inputs,
                ),
                (true, Some(1234567), true, &record.host_inputs),
                "prefix of {len} events"
            );
        }
    }

    /// A schedule prefix keeps the record's format and the jumps of global
    /// time its events reach, as analyze and bisect carry them: up to and
    /// including one at the prefix's end, none beyond it.
    #[test]
    fn a_schedule_prefix_keeps_the_jumps_its_events_reach() {
        let tid = DetTid::from_raw(3);
        let jump = |event: u64| TimeSkip {
            event,
            target: LogicalTime::from_nanos(1_000 * (event + 1)),
            reason: TimeSkipReason::IdleGuestPollers,
        };
        let record = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(tid, 10),
            SchedEvent::branches(tid, 20),
            SchedEvent::branches(tid, 30),
        ])
        .with_current_format()
        .with_time_skips(vec![jump(1), jump(2), jump(3)]);
        for (len, kept) in [
            (0, vec![]),
            (1, vec![jump(1)]),
            (2, vec![jump(1), jump(2)]),
            (9, vec![jump(1), jump(2), jump(3)]),
        ] {
            let prefix = record.schedule_prefix(len);
            assert!(prefix.lists_its_time_skips(), "prefix of {len} events");
            assert_eq!(
                prefix.time_skips().unwrap_or_default(),
                kept.as_slice(),
                "prefix of {len} events"
            );
        }
    }

    /// The timeslice a `--chaos-timeslice-draw=run` run drew is a setting of
    /// the whole run, as the other run settings are: a prefix of the
    /// schedule keeps it, whether it is shorter or whole.
    #[test]
    fn a_schedule_prefix_keeps_the_drawn_timeslice() {
        let tid = DetTid::from_raw(3);
        let record = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(tid, 10),
            SchedEvent::branches(tid, 20),
        ])
        .with_chaos_timeslice(Some(4321));
        for len in [0, 1, 2, 9] {
            assert_eq!(
                record.schedule_prefix(len).chaos_timeslice,
                Some(4321),
                "prefix of {len} events"
            );
        }
    }

    /// The rule that registered queued sleeps is a setting of the whole run,
    /// as the other run settings are: a prefix of the schedule keeps it,
    /// whether it is shorter or whole.
    #[test]
    fn a_schedule_prefix_keeps_the_sleep_registration_rule() {
        let tid = DetTid::from_raw(3);
        let record = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(tid, 10),
            SchedEvent::branches(tid, 20),
        ])
        .with_queued_sleeps_registered_at_first_pass(true);
        for len in [0, 1, 2, 9] {
            assert!(
                record
                    .schedule_prefix(len)
                    .queued_sleeps_registered_at_first_pass,
                "prefix of {len} events"
            );
        }
    }

    /// Every loader of a preemption record reads the same readmissions from
    /// the same bytes, whatever the record's format: the scheduler's
    /// (`read_recorded_rejoins`), the replay and editing reader
    /// (`Recording::decode`, `PreemptionReader::try_new`), the reader analyze,
    /// minimize and split-branches use (`PreemptionReader::new`), and
    /// bisect's (`serde_json` then `validate`). The records are shaped as
    /// main's writer at `fcd62385f` writes them: a schedule replay's
    /// re-record, which states format 3 and keeps its readmissions; a record
    /// from a writer that states no format, whose list starts empty; and a
    /// run's own, format 4. A record whose readmissions go backwards is
    /// refused by every one of them.
    #[test]
    fn every_loader_reads_the_same_readmissions_from_the_same_bytes() {
        use crate::scheduler::rejoin_log::Rejoin;
        let rejoin = r#"{"turn":4,"wakes":0,"calls":[[3,1]],"popped":[]}"#;
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "format-3 re-record",
                format!(
                    r#"{{"per_thread":{{}},"global":[],"format_version":3,"rejoins":[{rejoin}]}}"#
                ),
                Some(1),
            ),
            (
                "unformatted",
                r#"{"per_thread":{},"global":[],"rejoins":[]}"#.to_string(),
                Some(0),
            ),
            (
                "format 4",
                format!(
                    r#"{{"per_thread":{{}},"global":[],"format_version":4,"rejoins":[{rejoin}]}}"#
                ),
                Some(1),
            ),
            (
                "legacy",
                r#"{"per_thread":{},"global":[]}"#.to_string(),
                None,
            ),
        ];
        for (name, text, count) in cases {
            let path = dir.path().join("record.json");
            std::fs::write(&path, &text).unwrap();
            let scheduler = read_recorded_rejoins(&path).unwrap();
            let boundary_recording::Recording::Legacy(decoded) =
                boundary_recording::Recording::decode(text.as_bytes()).unwrap()
            else {
                panic!("{name}: not a legacy record");
            };
            let editor = PreemptionReader::try_new(&path).unwrap().into_inner();
            let analyze = PreemptionReader::new(&path).into_inner();
            let bisect: PreemptionRecord = serde_json::from_str(&text).unwrap();
            bisect.validate().unwrap();
            let all: [(&str, Option<Vec<Rejoin>>); 5] = [
                ("read_recorded_rejoins", scheduler),
                ("Recording::decode", decoded.rejoins),
                ("PreemptionReader::try_new", editor.rejoins),
                ("PreemptionReader::new", analyze.rejoins),
                ("serde_json and validate", bisect.rejoins),
            ];
            for (loader, rejoins) in &all {
                assert_eq!(rejoins.as_ref().map(Vec::len), count, "{name}: {loader}");
                assert_eq!(rejoins, &all[0].1, "{name}: {loader}");
            }
        }
        let backwards = r#"{"per_thread":{},"global":[],"format_version":3,"rejoins":[{"turn":9,"wakes":0,"calls":[]},{"turn":4,"wakes":0,"calls":[]}]}"#;
        let path = dir.path().join("backwards.json");
        std::fs::write(&path, backwards).unwrap();
        assert!(read_recorded_rejoins(&path).is_err());
        assert!(boundary_recording::Recording::decode(backwards.as_bytes()).is_err());
        assert!(PreemptionReader::try_new(&path).is_err());
        let bisect: PreemptionRecord = serde_json::from_str(backwards).unwrap();
        assert!(
            bisect
                .validate()
                .is_err_and(|error| error.contains("readmissions out of order")),
            "{:?}",
            bisect.validate()
        );
    }

    /// The readmissions of backgrounded calls name points of the recorded
    /// run, as the callback checkpoints do: a prefix of the whole schedule
    /// keeps them, and a shorter one drops them, so its replay readmits a
    /// finished call when it finds it finished.
    #[test]
    fn a_schedule_prefix_keeps_the_readmissions_only_for_the_whole_schedule() {
        let tid = DetTid::from_raw(3);
        let mut record = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(tid, 10),
            SchedEvent::branches(tid, 20),
        ]);
        record
            .rejoins
            .get_or_insert_with(Vec::new)
            .push(crate::scheduler::rejoin_log::Rejoin {
                turn: 4,
                wakes: 0,
                calls: vec![(tid, 1)],
                popped: None,
            });
        for len in [2, 9] {
            assert_eq!(
                record.schedule_prefix(len).rejoins,
                record.rejoins,
                "prefix of {len} events"
            );
        }
        for len in [0, 1] {
            assert!(
                record.schedule_prefix(len).rejoins.is_none(),
                "prefix of {len} events"
            );
        }
    }

    /// The background starts name points of the recorded run, as the
    /// readmissions do, and the record's format says it holds them all: a
    /// prefix of the whole schedule keeps both, and a shorter one drops the
    /// starts and states the format below theirs, as an edited record does,
    /// so its replay starts each call as before.
    #[test]
    fn a_schedule_prefix_keeps_the_background_starts_only_for_the_whole_schedule() {
        let tid = DetTid::from_raw(3);
        let mut record = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(tid, 10),
            SchedEvent::branches(tid, 20),
        ]);
        record.format_version = Some(PREEMPTION_RECORD_FORMAT_MAX);
        record.background_starts.push(BackgroundStart {
            after: 1,
            thread: tid,
            call: 1,
        });
        record.validate().unwrap();
        for len in [2, 9] {
            let prefix = record.schedule_prefix(len);
            assert_eq!(
                prefix.format_version, record.format_version,
                "prefix of {len} events"
            );
            assert_eq!(
                prefix.background_starts(),
                record.background_starts(),
                "prefix of {len} events"
            );
            prefix.validate().unwrap();
        }
        for len in [0, 1] {
            let prefix = record.schedule_prefix(len);
            assert_eq!(
                prefix.format_version,
                Some(BACKGROUND_STARTS_FORMAT - 1),
                "prefix of {len} events"
            );
            assert!(
                prefix.background_starts().is_empty(),
                "prefix of {len} events"
            );
            prefix.validate().unwrap();
        }
    }

    /// Whether the recorded run rewrote exclusive loops in memory is a setting
    /// a replay of the record must share, so a prefix record keeps it, for
    /// any length.
    #[test]
    fn a_schedule_prefix_keeps_the_in_memory_exclusive_rewrite_setting() {
        let tid = DetTid::from_raw(3);
        for on in [false, true] {
            let record = PreemptionRecord::from_sched_events(vec![
                SchedEvent::branches(tid, 10),
                SchedEvent::branches(tid, 20),
            ])
            .with_rewrite_exclusives_in_memory(on);
            for len in [0, 1, 2, 9] {
                assert_eq!(
                    record.schedule_prefix(len).rewrite_exclusives_in_memory(),
                    on,
                    "prefix of {len} events"
                );
            }
        }
    }

    /// The post-crash bound is a setting of the whole run, which every prefix
    /// of the schedule keeps; the switches the run took are points of it,
    /// which only a prefix of the whole schedule keeps (a shorter one lists
    /// none and is not checked against them).
    #[test]
    fn a_schedule_prefix_keeps_the_post_crash_bound_and_the_switches_only_for_the_whole_schedule() {
        let tid = DetTid::from_raw(3);
        let switch = crate::post_crash::PostCrashSwitch {
            dtid: tid,
            signal: libc::SIGABRT,
            exec_by: None,
            turn: 4,
            bound: 200_000_000,
        };
        let mut record = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(tid, 10),
            SchedEvent::branches(tid, 20),
        ])
        .with_post_crash_timeslice(Some(200_000_000));
        record.post_crash = Some(vec![switch]);
        for len in [0, 1, 2, 9] {
            assert_eq!(
                record.schedule_prefix(len).post_crash_timeslice(),
                Some(200_000_000),
                "prefix of {len} events"
            );
        }
        for len in [2, 9] {
            assert_eq!(
                record.schedule_prefix(len).post_crash_complete(),
                Some(&[switch][..]),
                "prefix of {len} events"
            );
        }
        for len in [0, 1] {
            assert_eq!(
                record.schedule_prefix(len).post_crash_complete(),
                None,
                "prefix of {len} events"
            );
        }
    }

    /// A replay of a schedule's first events keeps the callback checkpoints
    /// of the slices those events end, for the threads they run, and no later
    /// ones: the slice end at the prefix's own end belongs to the event after
    /// it. The result still validates, and lends its checkpoints to a
    /// schedule that replayed it and went on.
    #[test]
    fn a_schedule_prefix_keeps_the_checkpoints_of_the_slices_its_events_end() {
        use callback_history::Checkpoint;
        use callback_history::Origin;
        use callback_history::Site;

        use crate::slice_boundary::callbacks::Callback;
        let (main, other, late) = (
            DetTid::from_raw(3),
            DetTid::from_raw(5),
            DetTid::from_raw(7),
        );
        // Progress per thread: main 10, 11 (after the OtherInstructions), 21;
        // other 4; late 6.
        let mut record = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(main, 10),
            SchedEvent::branches(other, 4),
            SchedEvent {
                op: Op::OtherInstructions,
                ..SchedEvent::branches(main, 1)
            },
            SchedEvent::branches(main, 10),
            SchedEvent::branches(late, 6),
        ]);
        let mut point = |tid: DetTid, row: usize, progress: u64, last_op: Op| {
            let time = LogicalTime::from_nanos(100 * (row as u64 + 1));
            let history = record.per_thread.entry(tid).or_default();
            history.prio_changes.push((time, 1000));
            history.preemption_rcbs.push(progress);
            Checkpoint {
                history_index: row,
                origin: Origin {
                    callback: Callback::PmuTimer,
                    site: Site::Prehook,
                    progress,
                    last_op: Some(last_op),
                    time,
                    rcbs: progress,
                },
            }
        };
        let main_points = vec![
            point(main, 0, 10, Op::Branch),
            point(main, 1, 11, Op::OtherInstructions),
            point(main, 2, 21, Op::Branch),
            // A later callback at the same progress, after the schedule ends.
            point(main, 3, 21, Op::Branch),
        ];
        let other_points = vec![point(other, 0, 4, Op::Branch)];
        let late_points = vec![point(late, 0, 6, Op::Branch)];
        record
            .callback_checkpoints
            .insert(main, main_points.clone());
        record
            .callback_checkpoints
            .insert(other, other_points.clone());
        record
            .callback_checkpoints
            .insert(late, late_points.clone());
        record.validate().unwrap();

        // An exact interpolation keeps the histories/checkpoints themselves,
        // not a reconstructed record that happens to have the same events.
        let exact = record
            .interpolate_events(&record, record.global.clone())
            .unwrap();
        assert_eq!(
            serde_json::to_value(&exact).unwrap(),
            serde_json::to_value(&record).unwrap()
        );
        let mut other_record = record.clone();
        other_record.global.push(SchedEvent::branches(late, 1));
        let mut mixed = record.global.clone();
        mixed.push(SchedEvent::branches(main, 1));
        assert!(
            record
                .interpolate_events(&other_record, mixed)
                .unwrap_err()
                .contains("callback checkpoints")
        );

        let kept = |len: usize| {
            let prefix = record.schedule_prefix(len);
            prefix.validate().unwrap();
            assert_eq!(prefix.global, record.global[..len]);
            prefix.callback_checkpoints
        };
        assert!(kept(0).is_empty());
        // Each prefix of 1 to 4 events stops where its last event's slice
        // ends (main's at 10, other's at 4, main's at 11, main's two at 21),
        // so it drops those checkpoints and keeps the earlier ones.
        assert!(kept(1).is_empty());
        assert_eq!(kept(2), BTreeMap::from([(main, main_points[..1].to_vec())]));
        assert_eq!(
            kept(3),
            BTreeMap::from([
                (main, main_points[..1].to_vec()),
                (other, other_points.clone()),
            ])
        );
        assert_eq!(
            kept(4),
            BTreeMap::from([
                (main, main_points[..2].to_vec()),
                (other, other_points.clone()),
            ])
        );
        // The whole schedule has no event after it, nor has a longer prefix.
        assert_eq!(kept(5), record.callback_checkpoints);
        assert_eq!(
            record.schedule_prefix(9).callback_checkpoints,
            record.callback_checkpoints
        );

        // A schedule that replayed the prefix and went on takes the prefix's
        // checkpoints; one whose events they do not fit does not.
        let prefix = record.schedule_prefix(3);
        let mut continued = record.global[..3].to_vec();
        continued.push(SchedEvent::branches(other, 9));
        assert_eq!(
            prefix
                .with_schedule(continued.clone())
                .map(|record| (record.global, record.callback_checkpoints)),
            Some((continued, prefix.callback_checkpoints.clone()))
        );
        assert!(prefix.with_schedule(Vec::new()).is_none());
    }

    /// Each recorded callback checkpoint is placed at the thread's event whose
    /// running count reaches its progress, by its index in the global
    /// schedule, with other threads' events in between counted; a checkpoint
    /// before the thread's first event has no place
    /// (https://github.com/rrnewton/hermit/issues/4876).
    #[test]
    fn callback_checkpoints_are_placed_at_the_event_ending_their_slice() {
        use callback_history::Checkpoint;
        use callback_history::Origin;
        use callback_history::Site;

        use crate::slice_boundary::callbacks::Callback;
        let (a, b) = (DetTid::from_raw(3), DetTid::from_raw(5));
        let mut record = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(a, 4),
            SchedEvent::branches(b, 2),
            SchedEvent::branches(a, 6),
            SchedEvent::branches(b, 1),
        ]);
        let point = |history_index: usize, progress: u64| Checkpoint {
            history_index,
            origin: Origin {
                callback: Callback::Cpuid,
                site: Site::Prehook,
                progress,
                last_op: (progress > 0).then_some(Op::Branch),
                time: LogicalTime::from_nanos(progress * 10),
                rcbs: progress,
            },
        };
        record
            .callback_checkpoints
            .insert(a, vec![point(0, 4), point(1, 10)]);
        record.callback_checkpoints.insert(b, vec![point(0, 0)]);
        assert_eq!(
            record.callback_checkpoint_events(),
            BTreeMap::from([((a, 0), 0), ((a, 1), 2)])
        );
    }

    #[test]
    fn callback_checkpoint_metadata_validates_and_only_time_stripping_preserves_it() {
        use callback_history::Checkpoint;
        use callback_history::Origin;
        use callback_history::Site;
        use reverie::syscalls::Sysno;

        use crate::slice_boundary::callbacks::Callback;
        let tid = DetTid::from_raw(3);
        let mut record = PreemptionRecord::from_sched_events(vec![SchedEvent::branches(tid, 10)]);
        let time = LogicalTime::from_nanos(100);
        let mut history = ThreadHistory::new();
        history.prio_changes.push((time, 1000));
        history.preemption_rcbs.push(10);
        record.per_thread.insert(tid, history);
        let point = Checkpoint {
            history_index: 0,
            origin: Origin {
                callback: Callback::Syscall(Sysno::close),
                site: Site::Posthook,
                progress: 10,
                last_op: Some(Op::Branch),
                time,
                rcbs: 10,
            },
        };
        record.callback_checkpoints.insert(tid, vec![point.clone()]);
        record
            .rejoins
            .get_or_insert_with(Vec::new)
            .push(crate::scheduler::rejoin_log::Rejoin {
                turn: 4,
                wakes: 0,
                calls: vec![(tid, 1)],
                popped: None,
            });
        record.validate().unwrap();
        let mut stripped = record.clone();
        stripped.strip_event_times();
        assert_eq!(stripped.callback_checkpoints, record.callback_checkpoints);
        stripped.validate().unwrap();
        assert_eq!(
            record.normalize().callback_checkpoints,
            record.callback_checkpoints
        );
        for edited in [
            record.clone().strip_contents(),
            record.clone_preemptions_only(),
            record.with_latest_preempt_removed(),
        ] {
            assert!(edited.callback_checkpoints.is_empty());
            assert!(edited.rejoins.is_none());
        }
        let mut edited = record.clone();
        edited.split_map(|event, _| [event]);
        assert!(edited.callback_checkpoints.is_empty());
        assert!(edited.rejoins.is_none());
        let mut edited = record.clone();
        let _ = edited.schedevents_iter_mut();
        assert!(edited.callback_checkpoints.is_empty());
        assert!(edited.rejoins.is_none());
        let mut edited = record.clone();
        edited.preemptions_only();
        assert!(edited.callback_checkpoints.is_empty());
        assert!(edited.rejoins.is_none());
        for field in ["history_index", "progress", "time", "rcbs"] {
            let mut bad = record.clone();
            let point = &mut bad.callback_checkpoints.get_mut(&tid).unwrap()[0];
            match field {
                "history_index" => point.history_index = 1,
                "progress" => point.origin.progress = 9,
                "time" => point.origin.time = LogicalTime::from_nanos(101),
                "rcbs" => point.origin.rcbs = 11,
                _ => unreachable!(),
            }
            assert!(bad.validate().is_err(), "{field}");
        }
        let mut bad = record.clone();
        bad.callback_checkpoints.get_mut(&tid).unwrap().push(point);
        assert!(bad.validate().is_err());
        let encoded = serde_json::to_string(&record).unwrap();
        let decoded: PreemptionRecord = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, record);
        let empty = PreemptionRecord::from_sched_events(Vec::new());
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            r#"{"per_thread":{},"global":[]}"#
        );
        let old: PreemptionRecord =
            serde_json::from_str(r#"{"per_thread":{},"global":[]}"#).unwrap();
        assert_eq!(old, empty);
        let bad = encoded.replace("Posthook", "UnknownHook");
        assert!(serde_json::from_str::<PreemptionRecord>(&bad).is_err());
    }

    #[test]
    fn test_split_map() {
        let mut pr: PreemptionRecord = serde_json::from_str(
            r#"
            {
                "per_thread" : {
                },
                "global" : [
                    {
                        "dettid": 3,
                        "op": "OtherInstructions",
                        "count": 1,
                        "start_rip": null,
                        "end_rip": null,
                        "end_time": 946684799000000000
                    },
                    {
                        "dettid": 3,
                        "op": "Branch",
                        "count": 311,
                        "start_rip": null,
                        "end_rip": null,
                        "end_time": 946684799000003110
                    },
                    {
                        "dettid": 3,
                        "op": "OtherInstructions",
                        "count": 1,
                        "start_rip": null,
                        "end_rip": null,
                        "end_time": 946684799000003110
                    }
                ]
            }
        "#,
        )
        .unwrap();
        let original = pr.global.clone();
        pr.split_map(|e, _| vec![e]); // shouldn't change contents with this
        assert_eq!(original, pr.global);

        pr.split_map(|e, _| match &e {
            // this will split event with Op::Branch into two
            SchedEvent { op: Op::Branch, .. } => vec![e.clone(), e],
            _ => vec![e],
        });

        assert_eq!(
            pr.global.iter().map(|e| e.op).collect::<Vec<_>>(),
            vec![
                Op::OtherInstructions,
                Op::Branch,
                Op::Branch,
                Op::OtherInstructions
            ]
        );

        pr.split_map(|_, _| vec![]); // this will remove all entries
        assert_eq!(pr.global, Vec::new());
    }
}

// Reader and Writer implementations:
// =============================================================================

/// A writer for a stream of preemption events.
#[derive(Debug)]
pub struct PreemptionWriter {
    inner: PreemptionRecord,
    reserved_input_qualification: reserved_input::ProducerQualification,
    boundary_records: Option<Vec<detcore_model::schedule::boundary::Record>>,
    dest: Option<PathBuf>,
    flushed: bool,
}

impl PreemptionWriter {
    /// A new, empty record of preemptions, with an optional location on disk that it will be
    /// written to.  If no path is supplied, the results will accumulate in memory only.
    pub fn new(path: Option<PathBuf>) -> Self {
        PreemptionWriter {
            // A run logs where each backgrounded call rejoined (`push_rejoin`),
            // so its record says so even when no call went to the background.
            inner: PreemptionRecord {
                rejoins: Some(Vec::new()),
                ..Default::default()
            },
            reserved_input_qualification: Default::default(),
            boundary_records: None,
            dest: path,
            flushed: false,
        }
    }

    /// Observe the actual authenticated target-image setup through the local
    /// GlobalTool hook, never requested Config or a launcher bootstrap. A failed
    /// or unproved executed image permanently removes this producer's mark.
    pub(crate) fn observe_reserved_input_setup(
        &mut self,
        receipt: &reverie::reserved_input::InputSetupReceipt,
    ) -> Result<(), RecordAdmissionRefusal> {
        use reverie::reserved_input::InputAbi;
        use reverie::reserved_input::InputSetupOutcome;
        let layout = match &receipt.outcome {
            InputSetupOutcome::Qualified(layout) => {
                Some(ReservedInputLayout::from_authenticated_setup(
                    match layout.abi {
                        InputAbi::X86_64 => "x86_64",
                        InputAbi::Aarch64 => "aarch64",
                    },
                    layout.base,
                    layout.page_bytes as u64,
                ))
            }
            InputSetupOutcome::Unqualified(_) => None,
        };
        match layout.transpose() {
            Ok(layout) => {
                self.inner.reserved_input = self.reserved_input_qualification.observe(layout);
                Ok(())
            }
            Err(refusal) => {
                self.inner.reserved_input = self.reserved_input_qualification.observe(None);
                Err(refusal)
            }
        }
    }

    /// Write the record in `format` (`None` for format 1): the newest
    /// ([`PREEMPTION_RECORD_FORMAT_MAX`]) for a run that follows no record, or the
    /// format of the record a replay follows, whose rules it ran by. A writer
    /// notes no background starts until `noting_background_starts` says it
    /// does, so it states at most the format below
    /// [`BACKGROUND_STARTS_FORMAT`] ([`format_without_background_starts`]): a
    /// schedule replay's re-record of a record that notes its starts states
    /// the format below, the same timing rules without a list of starts. Until
    /// the writer records an instruction offset in a row, which raises it to
    /// [`INSTRUCTION_OFFSET_ROWS_FORMAT`] (`raise_format`), it states no format
    /// at or above that one either ([`format_without_instruction_offsets`]).
    pub fn with_format(mut self, format: Option<u32>) -> Self {
        self.inner.format_version =
            format_without_background_starts(format_without_instruction_offsets(format));
        self
    }

    /// Record a jump of global time (`PreemptionRecord::time_skips`), which
    /// puts the record in format 2 at least ([`TIME_SKIPS_FORMAT`]), never
    /// lowering a later format it states.
    pub fn insert_time_skip(&mut self, skip: TimeSkip) {
        if !self.inner.lists_its_time_skips() {
            self.inner.raise_format(TIME_SKIPS_FORMAT);
        }
        self.inner
            .time_skips
            .get_or_insert_with(Vec::new)
            .push(skip);
    }

    /// The position the next stored record takes in the recorded stream:
    /// where a jump made now falls (`TimeSkip::event`).
    pub fn stream_position(&self) -> u64 {
        let position = match &self.boundary_records {
            Some(records) => records.len(),
            None => self.inner.global.len(),
        };
        position as u64
    }

    /// Select the explicit boundary protocol before recording any global
    /// event. Metadata stays in the legacy history, with exactly one global
    /// stream in the new envelope. Existing writers remain legacy by default.
    pub fn with_slice_boundaries(mut self) -> Result<Self, String> {
        // A boundary replay takes no background starts
        // (`BoundaryRecording::new`), so a writer that noted one cannot
        // become a boundary writer, and a boundary writer states no format
        // that claims their list.
        if !self.inner.global.is_empty()
            || !self.inner.background_starts.is_empty()
            || self.boundary_records.is_some()
        {
            self.flushed = true;
            return Err("slice-boundary protocol must be selected before recording events".into());
        }
        self.inner.format_version = format_without_background_starts(self.inner.format_version);
        self.boundary_records = Some(Vec::new());
        Ok(self)
    }

    /// Whether this writer must retain the complete versioned boundary stream.
    pub fn records_slice_boundaries(&self) -> bool {
        self.boundary_records.is_some()
    }

    /// The next complete stored index, before accepting an annotation. The
    /// controller validates index/stack requests before consuming its cursor.
    pub(crate) fn next_boundary_index(&self) -> Option<usize> {
        self.boundary_records.as_ref().map(Vec::len)
    }

    /// Store the virtual-time epoch the recorded logical times are measured
    /// from, so a replay can start its clock at the same instant.
    pub fn with_epoch(mut self, epoch: DateTime<Utc>) -> Self {
        self.inner.epoch = Some(epoch);
        self
    }

    /// Store the run's `--pipe-wake-latency` (`None` for off, which leaves
    /// the record as it was before the field existed).
    pub fn with_pipe_wake_latency(mut self, latency: Option<u64>) -> Self {
        self.inner.pipe_wake_latency = latency;
        self
    }

    /// Store whether the run kept the signal-state copy
    /// (`Config::signal_state_copy`).
    pub fn with_signal_state_copy(mut self, on: bool) -> Self {
        self.inner.signal_state_copy = Some(on);
        self
    }

    /// Store the seed of the guest's randomness, so a replay can hand the guest
    /// the same values.
    pub fn with_rng_seed(mut self, rng_seed: u64) -> Self {
        self.inner.rng_seed = Some(rng_seed);
        self
    }

    /// Store whether the run charged a run of expired timed waits one turn
    /// (`Scheduler::expired_wait_runs_charged_once`; `false` leaves the record
    /// as it was before the field existed).
    pub fn with_expired_wait_runs_charged_once(mut self, once: bool) -> Self {
        self.inner.expired_wait_runs_charged_once = once;
        self
    }

    /// Store whether the run capped preemption charges
    /// (`--cap-preemption-charge`; off leaves the record as it was before
    /// the field existed).
    pub fn with_cap_preemption_charge(mut self, on: bool) -> Self {
        self.inner.cap_preemption_charge = on;
        self
    }

    /// Store the host files the run's guest opened, once the run has ended
    /// and before the record is flushed.
    pub fn set_host_inputs(&mut self, inputs: HostInputSet) {
        self.inner.host_inputs = Some(inputs);
    }

    /// Store whether the run rewrites exclusive loops in the guest's memory
    /// (`Config::rewrite_exclusives_in_memory`). On states the current
    /// rewriter's provenance with it (`with_rewrite_exclusives`); off
    /// clears any provenance, so use that setter for an overlay rewrite.
    pub fn with_rewrite_exclusives_in_memory(self, on: bool) -> Self {
        self.with_rewrite_exclusives(
            on.then(|| RewriteExclusives::current(RewriteExclusivesMode::Memory)),
        )
    }

    /// Store the exclusive-loop rewrite the run applies, if any
    /// (`RewriteExclusives`). The older in-memory field is set to agree.
    pub fn with_rewrite_exclusives(mut self, rewrite: Option<RewriteExclusives>) -> Self {
        self.inner.rewrite_exclusives = rewrite;
        self.inner.rewrite_exclusives_in_memory =
            rewrite.is_some_and(|rewrite| rewrite.in_memory());
        self
    }

    /// Store the store-exclusives that failed in the run (aarch64), counted
    /// once every thread has exited.
    pub fn record_store_exclusive_failures(&mut self, failures: u64) {
        self.inner.store_exclusive_failures = HostObservedCount(failures);
    }

    /// Store the `--max-timeslice` the run drew under `--chaos-timeslice-draw=run`
    /// (`Config::chaos_timeslice`; `None` writes nothing).
    pub fn with_chaos_timeslice(mut self, timeslice: Option<std::num::NonZeroU64>) -> Self {
        self.inner.chaos_timeslice = timeslice.map(std::num::NonZeroU64::get);
        self
    }

    /// Store how the run numbered its guest (`Config::guest_ids`), unless its
    /// PID namespace did, which leaves the record as it was before the field
    /// existed.
    pub fn with_guest_ids(mut self, ids: detcore_model::pid::GuestIds) -> Self {
        self.set_guest_ids(ids);
        self
    }

    /// [`Self::with_guest_ids`] on a writer in use: a run whose guest got a
    /// host ID after all states `GuestIds::Host`.
    pub fn set_guest_ids(&mut self, ids: detcore_model::pid::GuestIds) {
        self.inner.guest_ids = (ids != detcore_model::pid::GuestIds::Namespace).then_some(ids);
    }

    /// Store whether the run registered queued sleeps at the first pass
    /// (`Scheduler::queued_sleeps_registered_at_first_pass`; `false` leaves
    /// the record as it was before the field existed).
    pub fn with_queued_sleeps_registered_at_first_pass(mut self, first_pass: bool) -> Self {
        self.inner.queued_sleeps_registered_at_first_pass = first_pass;
        self
    }

    /// Store where a backgrounded call rejoined the run queue, so a replay
    /// can readmit it at the same turn (`crate::scheduler::rejoin_log`).
    pub(crate) fn push_rejoin(&mut self, rejoin: crate::scheduler::rejoin_log::Rejoin) {
        self.inner.rejoins.get_or_insert_with(Vec::new).push(rejoin);
    }

    /// Mark this record as written by a preemption replay that stopped
    /// following its record's readmissions, first at global event count
    /// `event` ([`PreemptionRecord::replay_departed_at`]): from there it
    /// readmitted finished calls by host timing, so this record is not an
    /// exact record of a run that followed its source. Warns once, naming the
    /// file, so nobody replays it as one.
    pub(crate) fn mark_replay_left_its_readmissions(&mut self, event: u64) {
        if self.inner.replay_departed_at.is_some() {
            return;
        }
        self.inner.replay_departed_at = Some(event);
        if let Some(dest) = &self.dest {
            tracing::warn!(
                "the preemption record {} was written by a replay that stopped following its \
                 source's readmissions at event {}; replay_departed_at preserves that \
                 provenance and faithful replay refuses it",
                dest.display(),
                event
            );
        }
    }

    /// Stamp the record with the current format: the run records its
    /// schedule without replaying one, so it notes every background start it
    /// takes (`insert_background_start`), and a record without any says its
    /// run began none. It states the newest format below
    /// [`INSTRUCTION_OFFSET_ROWS_FORMAT`] ([`format_without_instruction_offsets`]),
    /// which its first row with an instruction offset raises (`raise_format`).
    pub fn noting_background_starts(mut self) -> Self {
        let current = format_without_instruction_offsets(Some(PREEMPTION_RECORD_FORMAT_MAX));
        // A slice-boundary writer notes no starts (`insert_background_start`),
        // so it states the format below theirs.
        self.inner.format_version = if self.boundary_records.is_some() {
            format_without_background_starts(current)
        } else {
            current
        };
        self
    }

    /// Note that the scheduler just began `thread`'s kernel-run call `call`
    /// (a BACKGROUND commit), after the events recorded so far
    /// (`BackgroundStart`). Only a writer stamped by
    /// `noting_background_starts` takes it: another's record would say it
    /// notes starts it does not.
    pub(crate) fn insert_background_start(&mut self, thread: DetTid, call: u64) {
        if self.boundary_records.is_some() || !self.inner.notes_background_starts() {
            return;
        }
        self.inner.background_starts.push(BackgroundStart {
            after: self.inner.global.len() as u64,
            thread,
            call,
        });
    }

    /// Store the run's post-crash bound in effect
    /// (`Config::post_crash_timeslice_in_effect`; `None` for off).
    pub fn with_post_crash_timeslice(mut self, bound: Option<u64>) -> Self {
        self.inner.post_crash_timeslice = bound;
        self
    }

    /// Mark the record as listing every post-crash switch its run takes
    /// (possibly none): the scheduler's own writer, for a run with the switch
    /// on. A record stamped with the bound alone (analyze's derived records)
    /// lists none.
    pub fn noting_post_crash_switches(mut self) -> Self {
        self.inner.post_crash = Some(Vec::new());
        self
    }

    /// Store where the run took a post-crash switch.
    pub fn push_post_crash(&mut self, switch: crate::post_crash::PostCrashSwitch) {
        self.inner
            .post_crash
            .get_or_insert_with(Vec::new)
            .push(switch);
    }

    /// Does the record have zero entries?
    pub fn is_empty(&self) -> bool {
        self.inner.per_thread.is_empty()
    }

    /// Total number of elements in the record, across all threads.
    pub fn len(&self) -> usize {
        let mut count = 0;
        for v in self.inner.per_thread.values() {
            count += v.prio_changes.len() + v.chaos_epochs.len();
        }
        count
    }

    /// Register a thread with its default/final priority.
    pub fn register_thread(&mut self, tid: DetTid, prio: Priority) {
        if self
            .inner
            .per_thread
            .insert(
                tid,
                ThreadHistory {
                    final_prio: prio,
                    prio_changes: Vec::new(),
                    preemption_rcbs: Vec::new(),
                    preemption_instruction_offsets: Vec::new(),
                    chaos_epochs: Vec::new(),
                },
            )
            .is_some()
        {
            panic!(
                "PreemptionRecord: error, cannot re-register thread id already registered: {}",
                tid
            )
        }
    }

    /// Insert a new preemption point for the given thread.
    /// It must monotonically increase in time.
    ///
    /// For now it (redundantly) takes both the priority of the timeslice just finished,
    /// and the next one coming up.  This serves as a sanity check.
    ///
    /// `offset` says where the timeslice's timer was armed when
    /// `--chaos-instruction-offset` delivered it past its branch target.
    pub fn insert_reprioritization(
        &mut self,
        tid: DetTid,
        time: LogicalTime,
        rcbs: u64,
        prior_prio: Priority,
        next_prio: Priority,
        offset: Option<InstructionOffset>,
    ) {
        let history = self.inner.per_thread.get_mut(&tid).unwrap_or_else(|| {
            panic!(
                "PreemptionRecord: Cannot insert a preemption before registering thread {}",
                tid
            )
        });
        assert_eq!(history.final_prio, prior_prio);

        if let Some((last, _prio)) = history.prio_changes.last() {
            assert!(&time > last);
        }
        // The offsets stay empty until a timeslice has one, so a record
        // written without the option is the same as before it existed; the
        // first one fills in `None` for the rows before it.
        let offsets = &mut history.preemption_instruction_offsets;
        if offset.is_some() || !offsets.is_empty() {
            offsets.resize(history.prio_changes.len(), None);
            offsets.push(offset);
        }
        history.prio_changes.push((time, prior_prio));
        history.preemption_rcbs.push(rcbs);
        history.final_prio = next_prio;
        if offset.is_some() {
            self.inner.raise_format(INSTRUCTION_OFFSET_ROWS_FORMAT);
        }
    }

    /// Mark this record as written by a preemption replay that left a change
    /// point out of it, first at global event count `event`
    /// ([`PreemptionRecord::replay_departed_at`]). Warns once, naming the
    /// file, so nobody replays it as an exact record.
    pub fn mark_replay_departed(&mut self, event: u64) {
        if self.inner.replay_departed_at.is_some() {
            return;
        }
        self.inner.replay_departed_at = Some(event);
        if let Some(dest) = &self.dest {
            tracing::warn!(
                "the preemption record {} retains an omitted replay change point observed at source \
                 event {}; replay_departed_at preserves that provenance and faithful replay refuses it",
                dest.display(),
                event
            );
        }
    }

    /// The time of the last change point recorded for `tid`, if any. A new one
    /// must be strictly later ([`Self::insert_reprioritization`]).
    pub fn last_reprioritization_time(&self, tid: DetTid) -> Option<LogicalTime> {
        self.inner
            .per_thread
            .get(&tid)
            .and_then(|history| history.prio_changes.last())
            .map(|(time, _)| *time)
    }

    /// How many change points are recorded for `tid`.
    pub(crate) fn reprioritization_count(&self, tid: DetTid) -> usize {
        self.inner
            .per_thread
            .get(&tid)
            .map_or(0, |history| history.prio_changes.len())
    }

    /// Attach only an observed callback expiry to the row the original helper just wrote.
    pub(crate) fn insert_callback_checkpoint(
        &mut self,
        tid: DetTid,
        origin: callback_history::Origin,
    ) {
        let history = &self.inner.per_thread[&tid];
        let history_index = history
            .prio_changes
            .len()
            .checked_sub(1)
            .expect("callback follows priority row");
        assert_eq!(history.prio_changes[history_index].0, origin.time);
        assert_eq!(history.preemption_rcbs[history_index], origin.rcbs);
        self.inner
            .callback_checkpoints
            .entry(tid)
            .or_default()
            .push(callback_history::Checkpoint {
                history_index,
                origin,
            });
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Record a deterministic slowdown epoch transition for replay.
    pub fn insert_chaos_epoch(&mut self, tid: DetTid, transition: ChaosEpochTransition) {
        let history = self.inner.per_thread.get_mut(&tid).unwrap_or_else(|| {
            panic!(
                "PreemptionRecord: Cannot insert a chaos epoch before registering thread {}",
                tid
            )
        });
        if let Some(last) = history.chaos_epochs.last() {
            assert!(transition.logical_time > last.logical_time);
            assert!(transition.epoch > last.epoch);
        }
        history.chaos_epochs.push(transition);
    }

    /// Add a SchedEvent to the global log of thread behavior. A writer below
    /// [`STOPLESS_SLICE_ENDS_FORMAT`], which notes no background starts
    /// (`noting_background_starts`), or one that its first offset row raised
    /// past that format and that says it notes none
    /// (`PreemptionRecord::background_starts_unnoted`), records a timeslice
    /// end taken without a stop (`SchedEvent::stopless`) as an ordinary end:
    /// its record holds no marks, as a record lowered below its starts holds
    /// none (`PreemptionRecord::forget_stopless_marks_below_their_format`),
    /// and its replay stops at that end. The mark is dropped only when the
    /// finished record keeps the end visible without it; a writer whose
    /// next event would hide the end keeps the mark and its flush is
    /// refused, as a lowered record's is. A slice-boundary writer drops the
    /// mark at once: its stream is validated as a whole on read, where a
    /// mark in its format is refused (`boundary_recording`).
    pub fn insert_schedevent(&mut self, mut ev: SchedEvent) {
        if ev.stopless
            && self.boundary_records.is_some()
            && (self.inner.background_starts_unnoted
                || self
                    .inner
                    .format_version
                    .is_none_or(|format| format < STOPLESS_SLICE_ENDS_FORMAT))
        {
            ev.stopless = false;
        }
        if let Some(refusal) = ev.stopless_mark_refusal() {
            panic!("a record writer was given {ev:?}: {refusal}");
        }
        if let Some(records) = &mut self.boundary_records {
            assert!(
                ev.count > 0,
                "slice-boundary protocol refuses zero-count events"
            );
            records.push(detcore_model::schedule::boundary::Record::Event(ev));
            return;
        }
        if ev.count > 0 {
            self.inner.global.push(ev)
            // TODO, aggregation: possibly check if the last event was the SAME, and combine them,
            // increasing the count.
        } else {
            trace!("NOT recording scheduled event with zero count!");
        }
    }

    /// Append the observation of an existing slice-end request at its exact
    /// stream position. Refuse use with a legacy writer rather than silently
    /// dropping the boundary. Full stream ordering is checked before output.
    pub fn insert_slice_boundary(
        &mut self,
        boundary: detcore_model::schedule::boundary::Boundary,
    ) -> Result<usize, String> {
        let records = self
            .boundary_records
            .as_mut()
            .ok_or_else(|| "cannot append a slice boundary to a legacy recording".to_string())?;
        let index = records.len();
        boundary.validate_shape(index).map_err(|e| e.to_string())?;
        records.push(detcore_model::schedule::boundary::Record::TimesliceEnd(
            boundary,
        ));
        Ok(index)
    }

    /// Current recorded priority of a genuinely registered thread. During plain
    /// raw-schedule replay this is logical state, not a dispatch-control band.
    pub(crate) fn current_priority(&self, tid: DetTid) -> Priority {
        self.inner
            .per_thread
            .get(&tid)
            .unwrap_or_else(|| {
                panic!(
                    "PreemptionRecord: Cannot read current priority before registering thread {}",
                    tid
                )
            })
            .final_prio
    }

    /// Set the priority of the current thread after a change.
    pub fn set_current(&mut self, tid: DetTid, new_prio: Priority) {
        let history = self.inner.per_thread.get_mut(&tid).unwrap_or_else(|| {
            panic!(
                "PreemptionRecord: Cannot set current priority before registering thread {}",
                tid
            )
        });
        history.final_prio = new_prio;
    }

    /// Abort writing to disk and instead gather the output thusfar into a string.
    pub fn into_string(mut self) -> String {
        self.flushed = true;
        self.serialized_string()
            .expect("invalid preemption recording")
    }

    /// Finish any IO necessary to flush the record to disk.
    /// This will also happen automatically on drop.
    pub fn flush(mut self) -> Result<(), String> {
        self.flushed = true;
        self.write_to_disk()
    }

    /// Drop the `stopless` marks this writer may not hold (below
    /// [`STOPLESS_SLICE_ENDS_FORMAT`], or noting no background starts at
    /// the offsets' format) and refuse the record if a mark's
    /// slice end would become invisible without it
    /// (`PreemptionRecord::forget_stopless_marks_below_their_format`):
    /// such a record is invalid in its format, and writing it would
    /// silently lose the end.
    fn finalize(&mut self) -> Result<(), String> {
        self.inner.forget_stopless_marks_below_their_format();
        self.inner.stopless_lowering_refusal()
    }

    /// A bounded description for logs that do not request a complete JSON
    /// artifact. It streams the finalized record's INFO projection (the
    /// exact serialization minus the host-observed store-exclusive count,
    /// which compared evidence must not depend on) to a digest and counts
    /// rows without allocating its complete serialization or cloning the tape.
    pub(crate) fn summary(mut self) -> Result<String, String> {
        // Like into_string, this consumes the output owner without a later
        // Drop write. Run the same stopless lowering/refusal and boundary
        // validation as the write paths, even when no file is wanted:
        // a summary must not describe a record this writer would refuse
        // to write. Lowering drops only marks, never rows, so the counts
        // below are the record's either way.
        self.flushed = true;
        self.finalize()?;
        let digest = if let Some(records) = &self.boundary_records {
            let output = boundary_recording::BorrowedInfoEnvelope::new(&self.inner, records)
                .map_err(|error| error.to_string())?;
            streaming::digest_json(&output)
        } else {
            streaming::digest_json(&record_serde::InfoProjection(&self.inner))
        }
        .map_err(|error| error.to_string())?;
        let digest = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(format!(
            "{} threads, {} priority/epoch rows, {} legacy event rows, {} boundary-protocol rows, sha256:{digest}",
            self.inner.per_thread.len(),
            self.len(),
            self.inner.global.len(),
            self.boundary_records.as_ref().map_or(0, Vec::len)
        ))
    }

    fn write_to_disk(&mut self) -> Result<(), String> {
        if self.dest.is_some() {
            if self.boundary_records.is_none() {
                self.finalize()?;
                let path = self.dest.clone().unwrap();
                return self.inner.write_to_disk(&path);
            }
            let path = self.dest.clone().unwrap();
            // Lower and refuse stopless marks as the legacy path does,
            // before validating/serializing the complete stream:
            // BorrowedEnvelope validation alone neither drops the marks
            // lowering may drop nor is it the writer's ordering, and
            // write_file preflights serialization before it opens the
            // destination, so no refusal here creates or truncates it.
            self.finalize()?;
            let output = boundary_recording::BorrowedEnvelope::new(
                &self.inner,
                self.boundary_records.as_ref().unwrap(),
            )
            .map_err(|error| error.to_string())?;
            streaming::write_file(&path, &output).map_err(|e| {
                format!(
                    "Failed to write boundary recording {}: {e:#}",
                    path.display()
                )
            })
        } else {
            Err(
                "Cannot write_to_disk because this PreemptionWriter was created without a backing file.".to_string()
            )
        }
    }

    fn serialized_string(&self) -> Result<String, String> {
        let Some(records) = &self.boundary_records else {
            let mut record = self.inner.clone();
            record.forget_stopless_marks_below_their_format();
            record.stopless_lowering_refusal()?;
            return Ok(record.to_string());
        };
        // As the legacy branch above: serialize the lowered record, and
        // refuse one whose stopless end lowering would hide, rather than
        // stream it out under a format that may not hold the mark.
        let mut record = self.inner.clone();
        record.forget_stopless_marks_below_their_format();
        record.stopless_lowering_refusal()?;
        let output = boundary_recording::BorrowedEnvelope::new(&record, records)
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&output).map_err(|e| e.to_string())
    }
}

impl Drop for PreemptionWriter {
    fn drop(&mut self) {
        if !self.flushed
            && self.dest.is_some()
            && let Err(e) = self.write_to_disk()
        {
            panic!("Error while dropping PreemptionWriter: {}", e);
        }
    }
}

/// Read only whether a preemption record was written by a replay that left a
/// change point out of it ([`PreemptionRecord::replay_departed_at`]).
pub fn read_recorded_replay_departure(path: &Path) -> Result<Option<u64>, String> {
    #[derive(Deserialize)]
    struct DepartureOnly {
        #[serde(default)]
        replay_departed_at: Option<u64>,
    }
    Ok(boundary_recording::read_settings::<DepartureOnly>(path)?.replay_departed_at)
}

/// A reader for a stream of preemption events.
#[derive(Debug)]
pub struct PreemptionReader {
    inner: PreemptionRecord,
}

// Deprecated: use schedevents()
/// Read a full trace from disk.  Panic if it doesn't load.
pub fn read_trace(path: &Path) -> Vec<SchedEvent> {
    let pr = read_preemption_record(path);
    pr.global
}

fn read_preemption_record(path: &Path) -> PreemptionRecord {
    let parsed = File::open(path)
        .map_err(anyhow::Error::from)
        .and_then(|file| {
            let (record, _) = streaming::parse_utf8_pass::<PreemptionRecord, _>(file)?;
            record.validate().map_err(anyhow::Error::msg)?;
            Ok(record)
        });
    parsed.unwrap_or_else(|error| {
        panic!(
            "Error reading preemption record {}: {error:#}",
            path.display()
        )
    })
}

/// Read only the virtual-time epoch stored in a preemption record file.
///
/// Unlike [`PreemptionReader::new`], this reports an unreadable or malformed
/// file as an error instead of panicking, because the CLI consults it before
/// any guest starts. `Ok(None)` means the record predates stored epochs.
pub fn read_recorded_epoch(path: &Path) -> Result<Option<DateTime<Utc>>, String> {
    #[derive(Deserialize)]
    struct EpochOnly {
        #[serde(default)]
        epoch: Option<DateTime<Utc>>,
    }
    Ok(boundary_recording::read_settings::<EpochOnly>(path)?.epoch)
}

/// Read only the `--rng-seed` stored in a preemption record file, as
/// [`read_recorded_epoch`] reads the epoch. `Ok(None)` means the record predates
/// the field.
pub fn read_recorded_rng_seed(path: &Path) -> Result<Option<u64>, String> {
    #[derive(Deserialize)]
    struct RngSeedOnly {
        #[serde(default)]
        rng_seed: Option<u64>,
    }
    Ok(boundary_recording::read_settings::<RngSeedOnly>(path)?.rng_seed)
}

/// Whether any timeslice in a preemption record file was delivered past its
/// branch target (`--chaos-instruction-offset`). A replay of such a record
/// needs the precise single steps that only the ptrace backend takes.
pub fn read_recorded_instruction_offsets(path: &Path) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct OffsetsOnly {
        #[serde(default)]
        preemption_instruction_offsets: Vec<Option<InstructionOffset>>,
    }
    #[derive(Deserialize)]
    struct HistoriesOnly {
        #[serde(default)]
        per_thread: BTreeMap<DetTid, OffsetsOnly>,
    }
    Ok(boundary_recording::read_settings::<HistoriesOnly>(path)?
        .per_thread
        .values()
        .any(|history| {
            history
                .preemption_instruction_offsets
                .iter()
                .any(Option::is_some)
        }))
}

/// Read only the `--pipe-wake-latency` stored in a preemption record file, as
/// [`read_recorded_epoch`] reads the epoch. `Ok(None)` means the record was
/// made with the option off, or before the field existed.
pub fn read_recorded_pipe_wake_latency(path: &Path) -> Result<Option<u64>, String> {
    #[derive(Deserialize)]
    struct LatencyOnly {
        #[serde(default)]
        pipe_wake_latency: Option<u64>,
    }
    Ok(boundary_recording::read_settings::<LatencyOnly>(path)?.pipe_wake_latency)
}

/// The refusal of a replay whose `--pipe-wake-latency` differs from the one a
/// record stores, naming the record by `source` (an option and a path), or
/// `None` when they agree. Absent and off are the same; `Some(0)` is not off.
pub fn pipe_wake_latency_mismatch(
    source: &str,
    recorded: Option<u64>,
    replay: Option<u64>,
) -> Option<String> {
    let show = |latency: Option<u64>| match latency {
        Some(ns) => format!("--pipe-wake-latency={ns}"),
        None => "no --pipe-wake-latency".to_owned(),
    };
    (recorded != replay).then(|| {
        format!(
            "{source} was recorded with {} and would be replayed with {}. A hand-off moves \
             turns and the time they charge, so the replay cannot reproduce the run. Replay \
             with {}.",
            show(recorded),
            show(replay),
            show(recorded),
        )
    })
}

/// Read only whether the run that wrote a preemption record file capped
/// preemption charges, as [`read_recorded_epoch`] reads the epoch. A record
/// without the field was made with the option off.
pub fn read_recorded_cap_preemption_charge(path: &Path) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct CapOnly {
        #[serde(default)]
        cap_preemption_charge: bool,
    }
    Ok(boundary_recording::read_settings::<CapOnly>(path)?.cap_preemption_charge)
}

/// The refusal of a replay whose `--cap-preemption-charge` differs from the
/// one a record stores, naming the record by `source` (an option and a
/// path), or `None` when they agree.
pub fn cap_preemption_charge_mismatch(
    source: &str,
    recorded: bool,
    replay: bool,
) -> Option<String> {
    let show = |on: bool| {
        if on {
            "--cap-preemption-charge"
        } else {
            "no --cap-preemption-charge"
        }
    };
    (recorded != replay).then(|| {
        format!(
            "{source} was recorded with {} and would be replayed with {}. The option changes \
             what each preemption charges the clock, so timers and timeouts move and the replay \
             cannot reproduce the run. Replay with {}.",
            show(recorded),
            show(replay),
            show(recorded),
        )
    })
}

/// Read only the post-crash switches a preemption record holds, so a replay
/// can refuse to take a different one: `Some` (possibly empty) for a record
/// its own run wrote with the switch on, `None` for one made with it off,
/// derived from another, or made before the field existed.
pub fn read_recorded_post_crash_switches(
    path: &Path,
) -> Result<Option<Vec<crate::post_crash::PostCrashSwitch>>, String> {
    #[derive(Deserialize)]
    struct SwitchesOnly {
        #[serde(default)]
        post_crash: Option<Vec<crate::post_crash::PostCrashSwitch>>,
    }
    Ok(boundary_recording::read_settings::<SwitchesOnly>(path)?.post_crash)
}

/// Read only the post-crash bound stored in a preemption record file, as
/// [`read_recorded_epoch`] reads the epoch. `Ok(None)` means the record was
/// made with the switch off, or before the field existed.
pub fn read_recorded_post_crash_timeslice(path: &Path) -> Result<Option<u64>, String> {
    #[derive(Deserialize)]
    struct BoundOnly {
        #[serde(default)]
        post_crash_timeslice: Option<u64>,
    }
    Ok(boundary_recording::read_settings::<BoundOnly>(path)?.post_crash_timeslice)
}

/// The post-crash bound a replay of a record runs under, naming the record
/// by `source` (an option and a path) in a refusal. A record that stores a
/// bound gives it to a replay that sets none; a replay that sets another
/// bound, or sets one for a record made with the switch off, is refused: the
/// switch changes every slice after it, so the replay would leave the record.
pub fn post_crash_timeslice_for_replay(
    source: &str,
    recorded: Option<u64>,
    replay: Option<Option<u64>>,
) -> Result<Option<u64>, String> {
    let show = |bound: Option<u64>| match bound {
        Some(ns) => format!("--post-crash-timeslice={ns}"),
        None => "the post-crash switch off".to_owned(),
    };
    match (recorded, replay) {
        (recorded, None) => Ok(recorded),
        (recorded, Some(replay)) if recorded == replay => Ok(recorded),
        (recorded, Some(replay)) => Err(format!(
            "{source} was recorded with {} and would be replayed with {}. The switch changes \
             every slice after a crash, so the replay cannot reproduce the run. Replay with {}.",
            show(recorded),
            show(replay),
            show(recorded),
        )),
    }
}

/// Read only whether the signal-state copy was on in the run that wrote a
/// preemption record file, as [`read_recorded_epoch`] reads the epoch.
/// `Ok(None)` for a record written before the field existed.
pub fn read_recorded_signal_state_copy(path: &Path) -> Result<Option<bool>, String> {
    #[derive(Deserialize)]
    struct CopyOnly {
        #[serde(default)]
        signal_state_copy: Option<bool>,
    }
    Ok(boundary_recording::read_settings::<CopyOnly>(path)?.signal_state_copy)
}

/// Read only whether a preemption record file carries callback checkpoints
/// (`Config::annotated_priority_replay`), as [`read_recorded_epoch`] reads
/// the epoch. A record without the field, older or made without chaos
/// recording, carries none. Detcore validates the whole record, checkpoints
/// included, when it loads it for the run.
pub fn read_recorded_callback_checkpoints(path: &Path) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct CheckpointsOnly {
        #[serde(default)]
        callback_checkpoints: BTreeMap<String, Vec<serde_json::Value>>,
    }
    Ok(boundary_recording::read_settings::<CheckpointsOnly>(path)?
        .callback_checkpoints
        .values()
        .any(|points| !points.is_empty()))
}

/// Detcore's own refusal of a run prepared to replay a callback-annotated
/// priority history (`Config::annotated_priority_replay`) on an effective
/// configuration that cannot honor its row ends
/// (`Config::annotated_priority_replay_unqualified`), checked before the
/// scheduler or any history exists. Hosts refuse earlier, by name; this
/// backstop holds for every configuration that reaches Detcore, whoever
/// prepared it. `None` for an unannotated run or a qualified one.
pub fn annotated_replay_mode_refusal(cfg: &crate::Config) -> Option<String> {
    if !cfg.annotated_priority_replay {
        return None;
    }
    cfg.annotated_priority_replay_unqualified().map(|setting| {
        format!(
            "HERMIT_CALLBACK_CHECKPOINT_UNQUALIFIED_MODE: the run was prepared to replay a \
             callback-annotated priority history, but its configuration {setting}; no thread \
             has started"
        )
    })
}

/// Read only whether the run that wrote a preemption record file charged a
/// run of expired timed waits one turn, as [`read_recorded_epoch`] reads the
/// epoch. `Ok(false)` for a record that does not say: one written before the
/// field existed, or by a run under the `each` rule (by default race search),
/// both of which charged every wait.
pub fn read_recorded_expired_wait_runs_charged_once(path: &Path) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct ChargeOnly {
        #[serde(default)]
        expired_wait_runs_charged_once: bool,
    }
    Ok(boundary_recording::read_settings::<ChargeOnly>(path)?.expired_wait_runs_charged_once)
}

/// The refusal of a replay whose explicit `--expired-wait-charge` differs from
/// the rule a record states (`recorded_once`, as
/// [`read_recorded_expired_wait_runs_charged_once`] reads it), naming the
/// record by `source` (an option and a path), or `None` when they agree or the
/// replay leaves the option unset, and so takes the record's rule. For a
/// search (`hermit analyze`, `hermit bisect`), which runs every trial under
/// one rule, the remedy names the recordings instead.
pub fn expired_wait_charge_mismatch(
    source: &str,
    recorded_once: bool,
    replay: Option<detcore_model::config::ExpiredWaitCharge>,
    search: bool,
) -> Option<String> {
    let replay = replay?;
    let recorded = detcore_model::config::ExpiredWaitCharge::from_charged_once(recorded_once);
    (recorded != replay).then(|| {
        let remedy = if search {
            format!(
                "A search replays every trial under one rule, its target's unless its run \
                 arguments give one. Re-record this one under that rule \
                 (`hermit run --expired-wait-charge={} --record-preemptions-to=...`), or, if \
                 it is the target, pass --expired-wait-charge={} in the run arguments.",
                replay.as_str(),
                recorded.as_str()
            )
        } else {
            format!(
                "Replay with --expired-wait-charge={}, or leave the option out to take the \
                 record's.",
                recorded.as_str()
            )
        };
        format!(
            "{source} was recorded with --expired-wait-charge={} and would be replayed with \
             --expired-wait-charge={}. The rule decides how much virtual time expired timed \
             waits cost, and so which thread runs next, so the replay cannot reproduce the run. \
             {remedy}",
            recorded.as_str(),
            replay.as_str(),
        )
    })
}

/// Read only the timeslice a preemption record file states its run drew under
/// `--chaos-timeslice-draw=run` (`PreemptionRecord::chaos_timeslice`), as
/// [`read_recorded_epoch`] reads the epoch. `Ok(None)` for a record without
/// one.
pub fn read_recorded_chaos_timeslice(path: &Path) -> Result<Option<u64>, String> {
    #[derive(Deserialize)]
    struct TimesliceOnly {
        #[serde(default)]
        chaos_timeslice: Option<u64>,
    }
    Ok(boundary_recording::read_settings::<TimesliceOnly>(path)?.chaos_timeslice)
}

/// The `--max-timeslice` a chaos run with `--chaos-timeslice-draw=run` draws:
/// log-uniform over `range`, both bounds included, as a pure function of the
/// scheduler seed. A stream of its own (`CHAOS_TIMESLICE_SALT`) keeps it
/// independent of the seed's other draws, so adding the option changes no
/// other random choice of the run.
pub fn draw_chaos_timeslice(
    range: detcore_model::config::TimesliceRange,
    sched_seed: u64,
) -> std::num::NonZeroU64 {
    use rand::RngExt as _;
    use rand::SeedableRng as _;
    const CHAOS_TIMESLICE_SALT: u64 = 0x7469_6d65_736c_6963; // "timeslic"
    let mut prng = rand_pcg::Pcg64Mcg::seed_from_u64(sched_seed ^ CHAOS_TIMESLICE_SALT);
    let drawn = range.log_uniform(prng.random::<f64>()).round() as u64;
    std::num::NonZeroU64::new(drawn.clamp(range.min.get(), range.max.get()))
        .expect("the range starts at one RCB")
}

/// Read only whether the run that wrote a preemption record file registered
/// queued sleeps at the first pass, as [`read_recorded_epoch`] reads the
/// epoch. `Ok(false)` for a record that does not say: one written before the
/// field existed, or by a run under the `when-chosen` rule (by default race
/// search), both of which registered a sleep only when its thread was chosen.
pub fn read_recorded_queued_sleeps_registered_at_first_pass(path: &Path) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct RegistrationOnly {
        #[serde(default)]
        queued_sleeps_registered_at_first_pass: bool,
    }
    Ok(boundary_recording::read_settings::<RegistrationOnly>(path)?
        .queued_sleeps_registered_at_first_pass)
}

/// Whether `config` asks for queued sleeps to be registered at the first
/// pass, before the backend's limit: a replay that leaves
/// `--sleep-registration` unset takes its record's rule (a record that cannot
/// be read counts as `when-chosen`; the replay's own loader fails on it), and
/// any other run `Config::queued_sleeps_registered_at_first_pass`.
/// `Scheduler::new` registers at the first pass only when this holds and the
/// scheduler reads a sleeper's signal state
/// (`Config::reads_sleeper_signal_state`), and hermit refuses a launch where
/// this holds and that does not, before the guest starts.
pub fn requests_first_pass_sleep_registration(config: &crate::config::Config) -> bool {
    let replayed = config
        .replay_schedule_from
        .as_ref()
        .or(config.replay_preemptions_from.as_ref());
    match replayed {
        Some(path) if config.sleep_registration.is_none() && !config.recordreplay_modes => {
            read_recorded_queued_sleeps_registered_at_first_pass(path).unwrap_or(false)
        }
        _ => config.queued_sleeps_registered_at_first_pass(),
    }
}

/// The refusal of a replay whose explicit `--sleep-registration` differs from
/// the rule a record states (`recorded_first_pass`, as
/// [`read_recorded_queued_sleeps_registered_at_first_pass`] reads it), naming
/// the record by `source` (an option and a path), or `None` when they agree or
/// the replay leaves the option unset, and so takes the record's rule. For a
/// search (`hermit analyze`, `hermit bisect`), which runs every trial under
/// one rule, the remedy names the recordings instead.
pub fn sleep_registration_mismatch(
    source: &str,
    recorded_first_pass: bool,
    replay: Option<detcore_model::config::SleepRegistration>,
    search: bool,
) -> Option<String> {
    let replay = replay?;
    let recorded = detcore_model::config::SleepRegistration::from_registered_at_first_pass(
        recorded_first_pass,
    );
    (recorded != replay).then(|| {
        let remedy = if search {
            format!(
                "A search replays every trial under one rule, its target's unless its run \
                 arguments give one. Re-record this one under that rule \
                 (`hermit run --sleep-registration={} --record-preemptions-to=...`), or, if it \
                 is the target, pass --sleep-registration={} in the run arguments.",
                replay.as_str(),
                recorded.as_str()
            )
        } else {
            format!(
                "Replay with --sleep-registration={}, or leave the option out to take the \
                 record's.",
                recorded.as_str()
            )
        };
        format!(
            "{source} was recorded with --sleep-registration={} and would be replayed with \
             --sleep-registration={}. The rule decides when a sleeping thread's wake enters the \
             timed events, and so which timed event fires in each pass, so the replay cannot \
             reproduce the run. {remedy}",
            recorded.as_str(),
            replay.as_str(),
        )
    })
}

/// The refusal of a replay whose signal-state copy setting differs from the
/// one a record stores, naming the record by `source` (an option and a path),
/// or `None` when they agree or the record does not say.
pub fn signal_state_copy_mismatch(
    source: &str,
    recorded: Option<bool>,
    replay: bool,
) -> Option<String> {
    let show = |on: bool| {
        if on {
            "the signal-state copy on"
        } else {
            "the signal-state copy off (--no-signal-state-copy)"
        }
    };
    let recorded = recorded?;
    (recorded != replay).then(|| {
        format!(
            "{source} was recorded with {} and would be replayed with {}. The copy decides \
             some signal questions and adds a stop at each handler return, which can move a \
             preemption, so the replay cannot be relied on to reproduce the run. Replay with {}.",
            show(recorded),
            show(replay),
            show(recorded),
        )
    })
}

/// How the run that wrote the record at `path` numbered its guest, as the
/// record stores it: `GuestIds::Namespace` for a record that does not say,
/// which only a run its PID namespace numbered, or one before the field
/// existed, writes.
pub fn read_recorded_guest_ids(path: &Path) -> Result<detcore_model::pid::GuestIds, String> {
    #[derive(Deserialize)]
    struct IdsOnly {
        #[serde(default)]
        guest_ids: Option<detcore_model::pid::GuestIds>,
    }
    Ok(boundary_recording::read_settings::<IdsOnly>(path)?
        .guest_ids
        .unwrap_or_default())
}

/// The refusal of a replay whose guest would be numbered otherwise than the
/// run that a record (named by `source`, an option and a path) came from,
/// or `None` when they number it alike (`GuestIds::numbers_like`).
pub fn guest_ids_mismatch(
    source: &str,
    recorded: detcore_model::pid::GuestIds,
    replay: detcore_model::pid::GuestIds,
) -> Option<String> {
    (!recorded.numbers_like(&replay)).then(|| {
        format!(
            "{source} was recorded with guest process IDs {recorded}, and this replay's would be \
             {replay}. The record names every thread by its ID, so the replay cannot follow it."
        )
    })
}

/// Read only whether the run that wrote a preemption record file rewrote
/// exclusive loops in the guest's memory, as [`read_recorded_epoch`] reads
/// the epoch. A record without the field was made without the rewrite.
pub fn read_recorded_rewrite_exclusives_in_memory(path: &Path) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct RewriteOnly {
        #[serde(default)]
        rewrite_exclusives_in_memory: bool,
    }
    Ok(boundary_recording::read_settings::<RewriteOnly>(path)?.rewrite_exclusives_in_memory)
}

/// What a preemption record file states about its run's exclusive-loop
/// rewrite, for replay admission (`rewrite_exclusives_mismatch`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordedRewriteExclusives {
    /// The stated provenance, when the record carries one.
    pub rewrite: Option<RewriteExclusives>,
    /// The older in-memory field, the only rewrite a record made before
    /// the provenance could state.
    pub in_memory: bool,
    /// A record of the removed stripped-file heuristic rewrite
    /// (`rewrite_exclusives_stripped_heuristic`), whose rewritten
    /// instructions no supported rewrite reproduces.
    pub stripped_heuristic: bool,
}

/// Read only what a preemption record file states about its run's
/// exclusive-loop rewrite, as [`read_recorded_epoch`] reads the epoch.
pub fn read_recorded_rewrite_exclusives(path: &Path) -> Result<RecordedRewriteExclusives, String> {
    #[derive(Deserialize)]
    struct RewriteOnly {
        #[serde(default)]
        rewrite_exclusives: Option<RewriteExclusives>,
        #[serde(default)]
        rewrite_exclusives_in_memory: bool,
        #[serde(default)]
        rewrite_exclusives_stripped_heuristic: bool,
    }
    let read = boundary_recording::read_settings::<RewriteOnly>(path)?;
    Ok(RecordedRewriteExclusives {
        rewrite: read.rewrite_exclusives,
        in_memory: read.rewrite_exclusives_in_memory,
        stripped_heuristic: read.rewrite_exclusives_stripped_heuristic,
    })
}

/// The refusal of a replay whose exclusive-loop rewrite (`replay`, this
/// Hermit's current provenance, or `None` for no rewrite) is not the one
/// the record states, naming the record by `source` (an option and a
/// path); `None` when the replay can be shown to retire the same
/// instructions. A record of the removed stripped-file heuristic is
/// always refused, and so is a record that states no provenance — made
/// before records carried it, an in-memory one included — when the
/// replay rewrites at all: its rewritten instructions are unknown. Only
/// a replay that rewrites nothing still follows such a record, as it did
/// before the provenance existed, unless the record states the in-memory
/// rewrite.
pub fn rewrite_exclusives_mismatch(
    source: &str,
    recorded: RecordedRewriteExclusives,
    replay: Option<RewriteExclusives>,
) -> Option<String> {
    if recorded.stripped_heuristic {
        return Some(format!(
            "{source} was recorded with exclusive loops rewritten by the removed stripped-file \
             heuristic. No rewrite this Hermit applies reproduces the instructions that \
             heuristic wrote, so the replay cannot reproduce the run. Re-record the run with \
             --rewrite-exclusives."
        ));
    }
    let show = |rewrite: Option<RewriteExclusives>| match rewrite {
        Some(rewrite) => rewrite.describe(),
        None => "exclusive loops as the files have them".to_owned(),
    };
    if recorded.rewrite == replay {
        // Equal stated provenance, or neither side rewrites and the
        // record states none (checked against the older field below).
        if replay.is_some() || !recorded.in_memory {
            return None;
        }
    }
    if recorded.rewrite.is_none() && replay.is_some() {
        return Some(format!(
            "{source} does not state how its run rewrote exclusive loops (it was made before \
             records carried the rewrite's mode and format version), and this replay would \
             run with {}. Its rewritten instructions cannot be shown to be the recording's, \
             so the replay cannot reproduce the run. Re-record the run.",
            show(replay),
        ));
    }
    let recorded_show = match recorded.rewrite {
        Some(rewrite) => rewrite.describe(),
        None if recorded.in_memory => {
            "exclusive loops rewritten in the guest's memory by a rewrite format it does \
             not state"
                .to_owned()
        }
        None => show(None),
    };
    Some(format!(
        "{source} was recorded with {recorded_show} and would be replayed with {}. The \
         rewrite changes the instructions the guest retires, so the replay cannot reproduce \
         the run. Replay with {recorded_show}.",
        show(replay),
    ))
}

/// The refusal of a replay that would rewrite exclusive loops in the guest's
/// memory where the record's run did not, or the other way round, naming the
/// record by `source` (an option and a path); `None` when they agree.
pub fn rewrite_exclusives_in_memory_mismatch(
    source: &str,
    recorded: bool,
    replay: bool,
) -> Option<String> {
    let show = |on: bool| {
        if on {
            "exclusive loops rewritten in the guest's memory (--rewrite-exclusives)"
        } else {
            "exclusive loops as the files have them"
        }
    };
    (recorded != replay).then(|| {
        format!(
            "{source} was recorded with {} and would be replayed with {}. The rewrite changes \
             the instructions the guest retires, so the replay cannot reproduce the run. \
             Replay with {}.",
            show(recorded),
            show(replay),
            show(recorded),
        )
    })
}

// TODO: eventually this should do streaming IO, and abstract it behind this interface.
impl PreemptionReader {
    /// Fallible legacy access for editors. A boundary recording cannot be
    /// flattened into a legacy event vector, even if it has no markers.
    pub fn try_new(path: &Path) -> anyhow::Result<Self> {
        let inner =
            boundary_recording::Recording::read(path)?.into_legacy("legacy preemption reader")?;
        Ok(Self { inner })
    }

    /// Access the stored `PreemptionRecord`, and eagerly or lazily load its data from disk.
    pub fn new(path: &Path) -> Self {
        let pr = read_preemption_record(path);
        PreemptionReader { inner: pr }
    }

    /// Gets the inner `PreemptionRecord`.
    pub fn into_inner(self) -> PreemptionRecord {
        self.inner
    }

    /// Extract a copy of all the entries for a particular thread.  Note that this returns None if
    /// there is NO record for the thread (not registered).  Otherwise, it returns a copy of the
    /// `ThreadHistory`, which may be read into memory or not.
    pub fn extract_thread_record(&self, tid: &DetTid) -> Option<ThreadHistory> {
        self.inner.per_thread.get(tid).cloned()
    }

    /// Return the initial priority for a thread, if it is recorded in the record.
    pub fn thread_initial_priority(&self, tid: &DetTid) -> Option<Priority> {
        self.inner.per_thread.get(tid).map(|x| x.initial_priority())
    }

    /// Return all the threads in this record.
    pub fn all_threads(&self) -> Vec<DetTid> {
        // TODO(T110956298): have this return an iterator.
        self.inner.per_thread.keys().copied().collect()
    }

    /// Load the full record of preemptions into memory.
    pub fn load_all(&self) -> PreemptionRecord {
        self.inner.clone()
    }

    /// Size in number of preemptions, across all threads.
    pub fn size(&self) -> usize {
        let mut sum = 0;
        for th in self.inner.per_thread.values() {
            sum += th.prio_changes.len()
        }
        sum
    }
}

/// Take a file containing a PreemptionRecord and strip the times from all the events.
/// Optionally take a destination path, otherwise create a destination file in the same directory
/// with our own naming convention.
pub fn strip_times_from_events_file(
    sched_path: &Path,
    dest: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let new_path = dest.unwrap_or_else(|| sched_path.with_extension("notimes"));
    let mut recording = boundary_recording::Recording::read(sched_path)?;
    recording.strip_times();
    recording.write(&new_path)?;
    Ok(new_path)
}
