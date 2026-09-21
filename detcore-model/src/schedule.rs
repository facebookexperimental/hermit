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
// Scheduler events
//--------------------------------------------------------------------------------

/// A scheduled action by one thread in the system.  This can be recorded, or replayed to guide the
/// schedule.
#[derive(PartialEq, Debug, Eq, Clone, Hash, Serialize, Deserialize)]
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
    pub fn with_end_rip(mut self, end_rip: InstructionPointer) -> Self {
        self.end_rip = Some(end_rip);
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
