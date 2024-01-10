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
use serde::de;
use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;

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

/// Simply a type to hang Serialize/Deserialize instances off of.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Hash)]
pub struct SigWrapper(pub Signal);

impl Serialize for SigWrapper {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> de::Deserialize<'de> for SigWrapper {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct SignalVisitor;
        impl<'de> de::Visitor<'de> for SignalVisitor {
            type Value = Signal;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "string representing a Signal")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let sig: Result<Signal, String> =
                    FromStr::from_str(v).map_err(|e: nix::errno::Errno| e.to_string());
                sig.map_err(serde::de::Error::custom)
            }
        }

        let sig = deserializer.deserialize_str(SignalVisitor)?;
        Ok(SigWrapper(sig))
    }
}

impl FromStr for SigWrapper {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(SigWrapper(Signal::from_str(s)?))
    }
}

impl From<Signal> for SigWrapper {
    fn from(signal: Signal) -> Self {
        Self(signal)
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
///   > 1 to make sense, we need to include the full between-branches O's: "..BO*B..". We could
///   change this design by going to either extreme. (1) removing implicit O's and changing the
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
