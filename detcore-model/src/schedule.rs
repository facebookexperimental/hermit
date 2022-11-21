/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::num::NonZeroUsize;

use reverie_syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::pid::DetTid;
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

    /// Set the logical time.
    pub fn with_time(mut self, time: LogicalTime) -> SchedEvent {
        self.end_time = Some(time);
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

/// The observable operations that happen on a guest thread.  Each of these ultimately corresponds
/// to marker between two instructions.  As follows:
///
/// - Branches: after the  branch instruction has retired
/// - Syscall prehooks: just before the syscall instruction
/// - Syscall posthooks: just before the syscall instruction
/// - Rdtsc/Cpuid: just after the designated instruction
/// - OtherInstructions: just after the region of zero or more non-interceptable instructions.
///
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
    /// The only way to preempt inbewteen these is expensive single-stepping.
    OtherInstructions,
}
