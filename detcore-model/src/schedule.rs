/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::num::NonZeroUsize;
use std::str::FromStr;

use nix::sys::signal::Signal;
use reverie_syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use serde::de;

use crate::pid::DetTid;
use crate::time::DetTime;
use crate::time::LogicalDuration;
use crate::time::LogicalTime;

pub mod boundary;
// Scheduler events
//--------------------------------------------------------------------------------

/// Where a chaos timeslice with `--chaos-instruction-offset` ends: `instructions`
/// single steps after the thread's conditional-branch counter reaches
/// `rcb_target`. Straight-line code can return to an address without a
/// conditional branch (a repeated call, a backward jump, a `rep` instruction),
/// so the step count, not an address, names the point.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize, Hash)]
pub struct InstructionOffset {
    /// The thread's absolute PMU branch count the timer was armed for.
    pub rcb_target: u64,
    /// Single steps past that count, at least one.
    pub instructions: u64,
}

/// A scheduled action by one thread in the system.  This can be recorded, or replayed to guide the
/// schedule.
#[derive(PartialEq, Eq, Clone, Hash, Deserialize)]
pub struct SchedEvent {
    /// The thread that originated the event.
    pub dettid: DetTid,
    /// The operation performed by the thread.
    pub op: Op,
    /// The consecutive count of that same operation (run length encoding).
    pub count: u32,
    /// The instruction pointer before this batch of operations.
    pub start_rip: Option<InstructionPointer>,
    /// The instruction pointer after this batch of operations.
    pub end_rip: Option<InstructionPointer>,
    /// An optional snapshot of the thread logical time at this point.
    /// This includes time waiting on the global scheduler.
    pub end_time: Option<LogicalTime>,
    /// Set on the branches up to a timeslice end the run took without
    /// stopping the guest, because no other thread could run before the
    /// thread next entered Detcore (a lone thread's slice end). The guest was
    /// stopped later, so the event has no `end_rip`. A schedule replay may
    /// take such an end without a stop too, where its thread runs alone.
    #[serde(default)]
    pub stopless: bool,
    /// For a batch of branches that a timer armed with
    /// `--chaos-instruction-offset` ended past its branch target: where that
    /// timer was armed. A schedule replay arms the same timer for the event
    /// (`Some` only on an `Op::Branch` event, whose following
    /// `Op::OtherInstructions` event names the address it stopped at). `None`
    /// on every other event, and on every event of a schedule recorded without
    /// the option or before the field existed; a written schedule then omits
    /// it (see `Serialize`), so it is byte for byte what it was before.
    #[serde(default)]
    pub instruction_offset: Option<InstructionOffset>,
}

/// Written by hand so that a human-readable record (JSON) leaves `stopless`
/// out when it is false and `instruction_offset` out when it is `None`, as
/// every record made before those fields existed did, while a binary
/// encoding keeps every field: `SchedEvent` crosses Reverie's RPC, which is
/// bincode, positional and not self-describing, so a field skipped there
/// would misalign every field after it.
impl Serialize for SchedEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        // Destructured so that a new field cannot be left out unnoticed.
        let Self {
            dettid,
            op,
            count,
            start_rip,
            end_rip,
            end_time,
            stopless,
            instruction_offset,
        } = self;
        let binary = !serializer.is_human_readable();
        let written_stopless = *stopless || binary;
        let written_offset = instruction_offset.is_some() || binary;
        let mut state = serializer.serialize_struct(
            "SchedEvent",
            6 + usize::from(written_stopless) + usize::from(written_offset),
        )?;
        state.serialize_field("dettid", dettid)?;
        state.serialize_field("op", op)?;
        state.serialize_field("count", count)?;
        state.serialize_field("start_rip", start_rip)?;
        state.serialize_field("end_rip", end_rip)?;
        state.serialize_field("end_time", end_time)?;
        if written_stopless {
            state.serialize_field("stopless", stopless)?;
        } else {
            state.skip_field("stopless")?;
        }
        if written_offset {
            state.serialize_field("instruction_offset", instruction_offset)?;
        } else {
            state.skip_field("instruction_offset")?;
        }
        state.end()
    }
}

/// As derived, but without `stopless` when it is false and without
/// `instruction_offset` when it is `None`, so that the logs of a run with
/// neither read as before the fields existed: INFO and DEBUG lines that show
/// an event stay unchanged.
impl fmt::Debug for SchedEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            dettid,
            op,
            count,
            start_rip,
            end_rip,
            end_time,
            stopless,
            instruction_offset,
        } = self;
        let mut s = f.debug_struct("SchedEvent");
        s.field("dettid", dettid)
            .field("op", op)
            .field("count", count)
            .field("start_rip", start_rip)
            .field("end_rip", end_rip)
            .field("end_time", end_time);
        if *stopless {
            s.field("stopless", stopless);
        }
        if let Some(offset) = instruction_offset {
            s.field("instruction_offset", offset);
        }
        s.finish()
    }
}

/// A more compact printing.
impl fmt::Display for SchedEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(tid{}", self.dettid)?;
        if self.count > 1 {
            write!(f, " cnt={}", self.count)?;
        }
        if let Some(srip) = self.start_rip {
            write!(f, " strt={:#x}", srip)?;
        }
        if let Some(erip) = self.end_rip {
            write!(f, " end={:#x}", erip)?;
        }
        if let Some(time) = self.end_time {
            write!(f, " time={}", time)?;
        }
        if self.stopless {
            write!(f, " stopless")?;
        }
        if let Some(offset) = self.instruction_offset {
            write!(
                f,
                " offset=+{}@rcb{}",
                offset.instructions, offset.rcb_target
            )?;
        }
        write!(f, " {:?})", self.op)?;
        Ok(())
    }
}

impl SchedEvent {
    /// Add a syscall to the global scheduling history.  This takes the instruction pointer of the
    /// syscall itself.
    pub fn syscall(dettid: DetTid, sysno: Sysno, phase: SyscallPhase) -> SchedEvent {
        SchedEvent {
            dettid,
            op: Op::Syscall(sysno, phase),
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: None,
            stopless: false,
            instruction_offset: None,
        }
    }

    /// Add a batch of branches to the global scheduling history.
    pub fn branches(dettid: DetTid, count: u32) -> SchedEvent {
        SchedEvent {
            dettid,
            op: Op::Branch,
            count,
            start_rip: None, // TODO: track the start of the interval as well.
            end_rip: None,
            end_time: None,
            stopless: false,
            instruction_offset: None,
        }
    }

    /// Set the logical time directly.
    pub fn with_time(mut self, time: LogicalDuration) -> SchedEvent {
        self.end_time = Some(time);
        self
    }

    /// Correctly set logical time based on the threads current time.
    pub fn with_dettime(mut self, dt: &DetTime) -> SchedEvent {
        self.end_time = Some(dt.without_starting());
        self
    }

    /// Set the start_rip field.  The instruction pointer before the event began executing.
    pub fn with_start_rip(mut self, start_rip: InstructionPointer) -> Self {
        self.start_rip = Some(start_rip);
        self
    }

    /// Set the end_rip field.  The instruction pointer after the event completed.
    /// An end taken without a stop (`stopless`) has none
    /// ([`Self::stopless_mark_refusal`]).
    pub fn with_end_rip(mut self, end_rip: InstructionPointer) -> Self {
        assert!(
            !self.stopless,
            "a slice end without a stop has no RIP: {self:?}"
        );
        self.end_rip = Some(end_rip);
        self
    }

    /// Why this event's `stopless` mark contradicts the rest of it, if it
    /// does: only the branches up to a timeslice end are marked, and an end
    /// taken without a stop has no `end_rip`, since the guest was not stopped
    /// there, and no `instruction_offset`, since an offset is stepped from a
    /// stop at its branch target. The builders refuse each
    /// (`taken_without_stop`, `with_end_rip`, `with_instruction_offset`), and
    /// so do the record writer and reader, so that no record says an end was
    /// taken without a stop while it holds what only a stop provides.
    pub fn stopless_mark_refusal(&self) -> Option<&'static str> {
        if !self.stopless {
            None
        } else if self.op != Op::Branch {
            Some("only the branches up to a timeslice end may be marked `stopless`")
        } else if self.end_rip.is_some() {
            Some(
                "a timeslice end marked `stopless` has no end RIP: the guest was not stopped there",
            )
        } else if self.instruction_offset.is_some() {
            Some(
                "a timeslice end marked `stopless` has no instruction offset: an offset is stepped \
                 from a stop at its branch target",
            )
        } else {
            None
        }
    }

    /// Whether the two events are the same operation by the same thread at
    /// the same instruction pointers and, for a batch an instruction-offset
    /// timer ended, at the same offset, whichever way each run took a
    /// timeslice end there: a stop reads the `end_rip` that an end taken
    /// without one (`stopless`) does not have. An offset, like an end RIP,
    /// names where the batch ended, so two batches that differ only in their
    /// offsets are not the same kind. Counts and times are not compared. Not
    /// transitive (two stops at different RIPs are each the same kind as the
    /// end without one), so it compares two events and cannot key a hash.
    pub fn same_kind(&self, other: &SchedEvent) -> bool {
        let rips_known = !(self.stopless || other.stopless);
        (self.dettid, self.op, self.start_rip) == (other.dettid, other.op, other.start_rip)
            && (!rips_known || self.end_rip == other.end_rip)
            && self.instruction_offset == other.instruction_offset
    }

    /// [`SchedEvent::same_kind`], with the same count and time too: the same
    /// guest operations, however each run took a timeslice end there.
    pub fn same_operations(&self, other: &SchedEvent) -> bool {
        self.same_kind(other) && (self.count, self.end_time) == (other.count, other.end_time)
    }

    /// Mark these branches as ending at a timeslice end taken without a stop
    /// (`stopless`): they have no `end_rip`.
    pub fn taken_without_stop(mut self) -> Self {
        assert_eq!(self.op, Op::Branch, "only branches end at a timeslice end");
        assert!(
            self.end_rip.is_none(),
            "a slice end without a stop has no RIP"
        );
        assert!(
            self.instruction_offset.is_none(),
            "a slice end without a stop has no instruction offset"
        );
        self.stopless = true;
        self
    }

    /// Set the instruction_offset field: where the timer that ended this
    /// batch of branches was armed past its branch target.
    /// An end taken without a stop (`stopless`) has none
    /// ([`Self::stopless_mark_refusal`]).
    pub fn with_instruction_offset(mut self, offset: Option<InstructionOffset>) -> Self {
        assert!(
            !(self.stopless && offset.is_some()),
            "a slice end without a stop has no instruction offset: {self:?}"
        );
        self.instruction_offset = offset;
        self
    }
}

/// The type of the RIP value.
pub type InstructionPointer = NonZeroUsize;

/// Which phase of the syscall did we observe on a given event: the prehook or the posthook.
#[derive(PartialEq, Debug, Eq, Copy, Clone, Hash, Serialize, Deserialize)]
pub enum SyscallPhase {
    /// The event was recorded before physically beginning the syscall.
    Prehook,

    /// An internal (nonblocking) retry of the syscall to check if its done yet (but it wasn't).
    Polling,

    /// The event was recorded after the syscall logically completed.
    Posthook,
}

/// A signal the scheduler can name, INCLUDING the realtime signals `nix` cannot.
///
/// ⚠️ WHY THIS IS A RAW `i32` AND NOT A `nix::Signal`. It used to be
/// `SigWrapper(pub Signal)`, and `nix`'s `Signal` models only 1..=31. Every
/// cross-task notification path gated on `Signal::try_from(raw)`, so a
/// `tgkill`/`tkill`/`rt_tgsigqueueinfo`/`rt_sigqueueinfo` carrying
/// `SIGRTMIN..SIGRTMAX` delivered the signal to the target and then SILENTLY
/// skipped `NotifySignalPending`. Measured in-tree: the gate admitted exactly
/// 1..=31 and ZERO of the 31 realtime signals. A thread parked on
/// `ResourceID::WaitChild` was therefore never woken and the wait hung —
/// permanently, until the child exited or the thread was killed.
///
/// The two `rt_*sigqueueinfo` sites are the sharp end: they are reached from
/// `sigqueue()`/`pthread_sigqueue()`, which are used with realtime signals in
/// essentially all real code, so those notification call sites were wired up
/// and inert for their only normal use.
///
/// ⚠️ THE SERIALIZED FORM IS DELIBERATELY UNCHANGED FOR EVERY SIGNAL THAT COULD
/// ALREADY BE REPRESENTED. 1..=31 still render as the `nix` name (`"SIGUSR1"`),
/// byte-for-byte as before, so existing schedule files and DETLOG output do not
/// move. Only the previously-impossible values gain a spelling, `"SIG<n>"`, and
/// the deserializer accepts both. This is what keeps a model-type widening from
/// becoming a schedule-compatibility break.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Hash, PartialOrd, Ord)]
pub struct SigWrapper(pub i32);

impl SigWrapper {
    /// The raw signal number, always available.
    pub fn raw(&self) -> i32 {
        self.0
    }

    /// The `nix` signal, when one exists. `None` for realtime signals: they are
    /// real, deliverable, and simply unnamed by `nix`.
    pub fn signal(&self) -> Option<Signal> {
        Signal::try_from(self.0).ok()
    }

    /// How this signal is spelled in schedule files and DETLOG.
    pub fn as_string(&self) -> String {
        match self.signal() {
            Some(signal) => signal.as_str().to_string(),
            None => format!("SIG{}", self.0),
        }
    }
}

impl std::fmt::Display for SigWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_string())
    }
}

impl Serialize for SigWrapper {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.as_string())
    }
}

impl<'de> de::Deserialize<'de> for SigWrapper {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct SignalVisitor;
        impl<'de> de::Visitor<'de> for SignalVisitor {
            type Value = SigWrapper;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "string representing a signal")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                SigWrapper::from_str(v).map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_str(SignalVisitor)
    }
}

impl FromStr for SigWrapper {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        // The `nix` name first, so every previously-valid spelling keeps
        // parsing exactly as it did.
        if let Ok(signal) = Signal::from_str(s) {
            return Ok(SigWrapper(signal as i32));
        }
        // Then the realtime spelling this type adds.
        if let Some(rest) = s.strip_prefix("SIG")
            && let Ok(raw) = rest.parse::<i32>()
            && raw > 0
        {
            return Ok(SigWrapper(raw));
        }
        anyhow::bail!("not a signal: {s}")
    }
}

impl From<Signal> for SigWrapper {
    fn from(signal: Signal) -> Self {
        Self(signal as i32)
    }
}

#[cfg(test)]
mod sched_event_tests {
    use super::*;

    fn event(stopless: bool) -> SchedEvent {
        SchedEvent {
            end_time: Some(LogicalTime::from_nanos(1_250)),
            stopless,
            ..SchedEvent::branches(DetTid::from_raw(3), 100)
        }
    }

    /// A record without slice ends taken without a stop is written exactly as
    /// before the field existed, and a record written before then reads as
    /// one whose every slice end had its stop.
    #[test]
    fn json_leaves_out_a_false_stopless_and_reads_its_absence_as_false() {
        let before = r#"{"dettid":3,"op":"Branch","count":100,"start_rip":null,"end_rip":null,"end_time":1250}"#;
        assert_eq!(serde_json::to_string(&event(false)).unwrap(), before);
        assert_eq!(
            serde_json::from_str::<SchedEvent>(before).unwrap(),
            event(false)
        );
        let stopless = serde_json::to_string(&event(true)).unwrap();
        assert_eq!(
            stopless,
            r#"{"dettid":3,"op":"Branch","count":100,"start_rip":null,"end_rip":null,"end_time":1250,"stopless":true}"#
        );
        assert_eq!(
            serde_json::from_str::<SchedEvent>(&stopless).unwrap(),
            event(true)
        );
    }

    /// The RPC's encoding is positional: the field is written whatever its
    /// value, and both values come back.
    #[test]
    fn a_binary_encoding_keeps_stopless_in_place() {
        for stopless in [false, true] {
            let pair = (event(stopless), 7u32);
            let wire = bincode::serde::encode_to_vec(&pair, bincode::config::legacy()).unwrap();
            let (back, _): ((SchedEvent, u32), usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(back, pair);
        }
    }

    /// A stop and a stopless end at the same branch count are the same
    /// operations; any other difference is not.
    #[test]
    fn same_operations_ignores_only_how_a_slice_end_was_taken() {
        let stopped = SchedEvent {
            end_rip: InstructionPointer::new(0x1000),
            ..event(false)
        };
        assert!(stopped.same_operations(&event(true)));
        assert!(event(true).same_operations(&stopped));
        assert!(stopped.same_operations(&stopped.clone()));
        let elsewhere = SchedEvent {
            end_rip: InstructionPointer::new(0x2000),
            ..stopped.clone()
        };
        assert!(!stopped.same_operations(&elsewhere));
        let later = SchedEvent {
            count: 101,
            ..event(true)
        };
        assert!(!stopped.same_operations(&later));
        // An instruction offset names where the batch ended, like an end RIP,
        // so batches that differ only in their offsets are not the same.
        let offset = |instructions| {
            Some(InstructionOffset {
                rcb_target: 98,
                instructions,
            })
        };
        let at_offset = stopped.clone().with_instruction_offset(offset(3));
        assert!(at_offset.same_operations(&at_offset.clone()));
        assert!(!at_offset.same_operations(&stopped));
        assert!(!stopped.same_kind(&at_offset));
        assert!(!at_offset.same_kind(&stopped.clone().with_instruction_offset(offset(4))));
    }

    #[test]
    fn debug_shows_stopless_only_when_set() {
        let plain = format!("{:?}", event(false));
        assert_eq!(
            plain,
            "SchedEvent { dettid: DetPid(3), op: Branch, count: 100, start_rip: None, end_rip: None, end_time: Some(LogicalTime(1250)) }"
        );
        assert_eq!(
            format!("{:?}", event(true)),
            plain.replace(" }", ", stopless: true }")
        );
    }

    #[test]
    #[should_panic(expected = "a slice end without a stop has no RIP")]
    fn a_slice_end_without_a_stop_has_no_rip() {
        let _ = SchedEvent::branches(DetTid::from_raw(3), 1)
            .with_end_rip(InstructionPointer::new(0x1000).unwrap())
            .taken_without_stop();
    }

    /// Nor can a RIP be set on an end already marked as taken without a stop,
    /// in the other order.
    #[test]
    #[should_panic(expected = "a slice end without a stop has no RIP")]
    fn a_rip_cannot_be_set_on_a_slice_end_taken_without_a_stop() {
        let _ = SchedEvent::branches(DetTid::from_raw(3), 1)
            .taken_without_stop()
            .with_end_rip(InstructionPointer::new(0x1000).unwrap());
    }
}

#[cfg(test)]
mod sigwrapper_tests {
    use super::*;

    /// ⚠️ THE COMPATIBILITY CLAIM, DEMONSTRATED RATHER THAN ASSERTED.
    ///
    /// Widening a serialized model type is only safe if every value that could
    /// ALREADY be written still round-trips to the same bytes. This walks all 31
    /// signals `nix` can name and requires the serialized form to be exactly the
    /// `nix` name — which is what the old `serialize_str(self.0.as_str())`
    /// produced — so no existing schedule file or DETLOG line moves.
    #[test]
    fn every_previously_representable_signal_serializes_byte_identically() {
        for raw in 1..=31i32 {
            let Ok(signal) = Signal::try_from(raw) else {
                continue;
            };
            let wrapper = SigWrapper::from(signal);
            let json = serde_json::to_string(&wrapper).expect("serialize");
            // Exactly what the pre-widening implementation emitted.
            let expected = serde_json::to_string(signal.as_str()).expect("serialize name");
            assert_eq!(
                json,
                expected,
                "signal {raw} ({}) changed its serialized form",
                signal.as_str()
            );
        }
    }

    /// The previously-impossible values gain a spelling, and it does not collide
    /// with any `nix` name.
    #[test]
    fn realtime_signals_gain_a_distinct_spelling() {
        for raw in 32..=64i32 {
            let wrapper = SigWrapper(raw);
            assert_eq!(wrapper.as_string(), format!("SIG{raw}"));
            assert_eq!(wrapper.signal(), None, "nix must still not name {raw}");
        }
    }

    /// Round-trip in both directions, over the whole space this type now models.
    #[test]
    fn every_signal_round_trips_through_serde_and_fromstr() {
        for raw in 1..=64i32 {
            let wrapper = SigWrapper(raw);
            let json = serde_json::to_string(&wrapper).expect("serialize");
            let back: SigWrapper = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, wrapper, "serde round-trip lost signal {raw}");
            let parsed = SigWrapper::from_str(&wrapper.as_string()).expect("from_str");
            assert_eq!(parsed, wrapper, "FromStr round-trip lost signal {raw}");
        }
    }

    /// A reader written before the widening emitted names; a reader after it may
    /// emit either. Both spellings must parse, or an old schedule file stops
    /// loading.
    #[test]
    fn both_spellings_deserialize() {
        let by_name: SigWrapper = serde_json::from_str("\"SIGUSR1\"").expect("name");
        assert_eq!(by_name, SigWrapper::from(Signal::SIGUSR1));
        let by_number: SigWrapper = serde_json::from_str("\"SIG40\"").expect("number");
        assert_eq!(by_number, SigWrapper(40));
    }
}

#[cfg(test)]
mod instruction_offset_tests {
    use super::*;

    /// `SchedEvent` as it was before `instruction_offset` existed, with the
    /// derived `Serialize` and `Debug` it had then.
    mod before {
        use super::*;

        #[derive(Debug, Serialize)]
        pub struct SchedEvent {
            pub dettid: DetTid,
            pub op: Op,
            pub count: u32,
            pub start_rip: Option<InstructionPointer>,
            pub end_rip: Option<InstructionPointer>,
            pub end_time: Option<LogicalTime>,
        }
    }

    fn branch(offset: Option<InstructionOffset>) -> SchedEvent {
        SchedEvent::branches(DetTid::from_raw(3), 7)
            .with_end_rip(NonZeroUsize::new(4096).unwrap())
            .with_time(LogicalTime::from_nanos(70))
            .with_instruction_offset(offset)
    }

    /// An event without an instruction offset is written, and printed, exactly
    /// as it was before the field existed; one with an offset keeps it through
    /// JSON, and the RPC's encoding carries the field either way. A schedule
    /// written before the field existed reads as events without offsets.
    #[test]
    fn an_absent_offset_leaves_an_event_unchanged_and_a_present_one_round_trips() {
        let plain = branch(None);
        let old = before::SchedEvent {
            dettid: plain.dettid,
            op: plain.op,
            count: plain.count,
            start_rip: plain.start_rip,
            end_rip: plain.end_rip,
            end_time: plain.end_time,
        };
        let json = serde_json::to_string(&plain).unwrap();
        assert_eq!(json, serde_json::to_string(&old).unwrap());
        assert_eq!(
            serde_json::to_string_pretty(&plain).unwrap(),
            serde_json::to_string_pretty(&old).unwrap()
        );
        assert_eq!(serde_json::from_str::<SchedEvent>(&json).unwrap(), plain);
        assert_eq!(format!("{plain:?}"), format!("{old:?}"));
        assert_eq!(format!("{plain:#?}"), format!("{old:#?}"));
        assert!(!format!("{plain}").contains("offset"));

        let offset = InstructionOffset {
            rcb_target: 40,
            instructions: 5,
        };
        let with = branch(Some(offset));
        let json = serde_json::to_string(&with).unwrap();
        assert!(
            json.ends_with(r#","instruction_offset":{"rcb_target":40,"instructions":5}}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<SchedEvent>(&json).unwrap(), with);
        assert_ne!(with, plain);
        assert!(format!("{with:?}").ends_with(
            "instruction_offset: InstructionOffset { rcb_target: 40, instructions: 5 } }"
        ));
        assert!(format!("{with}").contains(" offset=+5@rcb40 "), "{with}");

        for event in [plain, with] {
            let config = bincode::config::standard();
            let encoded = bincode::serde::encode_to_vec(&event, config).unwrap();
            let (decoded, _): (SchedEvent, usize) =
                bincode::serde::decode_from_slice(&encoded, config).unwrap();
            assert_eq!(decoded, event);
        }
    }
}

/// NOTE [Event Semantics]
///
/// The observable operations that happen on a guest thread.
///
/// Each Op event has a beginning and an end, containing an interval of zero or more instructions
/// inbetween. Each beginning and end point in time can be thought of as an imaginary marker between
/// two instructions.  Start/end RIP values, if present in the containing `SchedEvent`, correspond
/// to those beginning/end points and always point to the *next* instruction to execute.
///
/// If we speak of an event as an instantaneous thing, we're usually thinking of it as its end
/// marker. Which is as follows for each:
///
/// - Branches: after the  branch instruction has retired
/// - Syscall prehooks: just before the syscall instruction, after whatever came before
/// - Syscall posthooks: just after the syscall instruction completes
/// - Rdtsc/Cpuid: just after the designated instruction
/// - OtherInstructions: just after the region of zero or more branch-free, non-interceptable
///   instructions.
/// - SignalReceived: just after the last regular, pre-signal guest instruction, and just before the
///   first instruction of the signal handler.
///
/// If we view each event as a series of instructions contained between its start/end markers, then
/// the pattern of instructions for each would be as follows.  Here we use simple regular
/// expressions with "B" standing for branch instructions, "S" for syscall instructions, "R" for
/// RDTSC, "C" for CPUID, and "O" for all other instructions.
///
/// - Branch "O*B"
/// - Syscall prehook: ""
/// - Syscall posthook: "S"
/// - Rdtsc: "R"
/// - Cpuid: "C"
/// - OtherInstructions: "O*"
/// - SignalReceived: ""
///
/// A few observations about the above:
///
/// - Some events always correspond to zero instructions.
/// - OtherInstructions are omnipresent "dark matter" that we cannot intercept or count, so are
///   implicitly present between other events.
/// - Therefore the OtherInstructions event itself is only interesting insofar as it signals the
///   absence of other events.
/// - Branches include an implicit prefix of OtherInstructions.  This is because for a branch count
///   greater than 1 to make sense, we need to include the full between-branches O's: "..BO*B..". We
///   could change this design by going to either extreme. (1) removing implicit O's and changing the
///   count mechanism to allow repetition of entire sequences "(O*B)^3" instead of "B^3".  Or (2),
///   including implicit O's in all event types, and not recording them explicitly.
#[derive(PartialEq, Debug, Eq, Copy, Clone, Hash, Serialize, Deserialize)]
pub enum Op {
    /// A single retired conditional branch, corresponding to one increment of the RCB counter.
    Branch,

    /// A nondeterministic rdtsc instruction.
    Rdtsc,

    /// A nondeterministic cpuid instruction.
    Cpuid,

    /// A system call performed by the thread.  The bool is set to true when this is a syscall
    /// PREHOOKh event, which is recorded BEFORE the syscall instruction executes, rather than
    /// after.
    Syscall(Sysno, SyscallPhase),

    /// An unknown number of other instructions that occured BETWEEN hermit-interceptable events.
    /// The only way to preempt in between these is expensive single-stepping.
    OtherInstructions,

    /// The point a signal handler is received, just after whatever regular user instruction
    /// preceeded it, and just before the first instruction of the signal handler.
    SignalReceived(SigWrapper),
}
