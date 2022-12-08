/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;

use detcore_model::collections::ReplayCursor;
use detcore_model::pid::DetTid;
use detcore_model::schedule::Op;
use detcore_model::schedule::SchedEvent;
use detcore_model::schedule::SyscallPhase;
use detcore_model::time::LogicalTime;
use tracing::debug;
use tracing::info;
use tracing::trace;

/// Replayer
#[derive(Debug)]
pub struct Replayer {
    /// A cursor that holds our place in the global total order of events being replayed.
    pub cursor: ReplayCursor<SchedEvent>,
    /// Keep track of how many events we have replayed.  The current value is the event number of
    /// the NEXT event to replay
    pub traced_event_count: u64,
    /// Desync events observed per thread.  Counts (soft,hard) desyncs.
    pub desync_counts: BTreeMap<DetTid, (u64, u64)>,
    /// A cached copy of the same (immutable) field in Config.
    pub replay_exhausted_panic: bool,
    /// A cached copy of the same (immutable) field in Config.
    pub die_on_desync: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopReason {
    FatalDesync,
    ReplayExausted,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplayAction {
    Continue(Option<LogicalTime>),
    Stop(StopReason),
    ContextSwitch(bool, DetTid, Option<LogicalTime>),
}

fn compare_desync(observed: &SchedEvent, expected: &SchedEvent) -> String {
    if !is_desync(observed, expected) {
        "MATCHED".to_string()
    } else if observed.op != expected.op {
        "FULL-OP-DESYNC".to_string()
    } else {
        let mut msg = "DESYNC".to_string();
        if (expected.start_rip.is_some() && observed.start_rip != expected.start_rip)
            || (expected.end_rip.is_some() && observed.end_rip != expected.end_rip)
        {
            msg = "RIP-".to_owned() + &msg;
        }
        if observed.end_time != expected.end_time {
            msg = "TIME-".to_owned() + &msg;
        }
        if observed.op == Op::Branch
            && expected.op == Op::Branch
            && observed.count != expected.count
        {
            msg = "RCB-".to_owned() + &msg;
        }
        msg
    }
}

fn is_desync(observed: &SchedEvent, expected: &SchedEvent) -> bool {
    if (observed.dettid, observed.op, observed.count)
        != (expected.dettid, expected.op, expected.count)
    {
        return true;
    }
    // It is ok for the observed event to contain information not present in the replay trace.
    if expected.end_time.is_some() && expected.end_time != observed.end_time {
        return true;
    }

    if observed.start_rip != expected.start_rip && expected.start_rip.is_some() {
        return true;
    }

    if observed.end_rip != expected.end_rip && expected.end_rip.is_some() {
        return true;
    }

    false
}

fn is_hard_desync(observed: &SchedEvent, expected: &SchedEvent) -> bool {
    let mut strip1 = observed.clone();
    let mut strip2 = expected.clone();
    strip1.end_time = None;
    strip2.end_time = None;
    strip1.count = 0;
    strip2.count = 0;
    strip1 != strip2
}

impl Replayer {
    /// This function look-aheads into following instruction to determine if
    /// an explicit timer preemption is required while replaying. This is required when
    /// the original run schedule was modified before replaying which should be a valid
    /// usecase for hermit analyze. Returned LogicalTime is RELATIVE to the corresponding thread localtime
    fn resolve_time_to_run(&self, next: &SchedEvent) -> Option<LogicalTime> {
        let cursor = &self.cursor;

        if let Some(next_time_slice_in_brances) =
            match (next, cursor.peek_nth(1), cursor.peek_nth(2)) {
                (
                    SchedEvent {
                        op: Op::Branch,
                        count,
                        ..
                    },
                    Some(SchedEvent {
                        op: Op::OtherInstructions,
                        ..
                    }),
                    Some(SchedEvent { op: Op::Branch, .. }),
                )
                | (
                    // this is currently impossible because we always have OtherInstruction
                    // following a branch but this doesn't have to be the case and it might
                    // be better to avoid this behavior in the future
                    SchedEvent {
                        op: Op::Branch,
                        count,
                        ..
                    },
                    Some(SchedEvent { op: Op::Branch, .. }),
                    _,
                ) => {
                    trace!("branch count to set the next_timeslice: {}", count);
                    Some(count)
                }
                (
                    SchedEvent {
                        op: Op::Branch,
                        count,
                        dettid: tid,
                        ..
                    },
                    Some(SchedEvent {
                        op: Op::OtherInstructions,
                        ..
                    }),
                    Some(SchedEvent {
                        dettid: next_tid, ..
                    }),
                )
                | (
                    SchedEvent {
                        op: Op::Branch,
                        count,
                        dettid: tid,
                        ..
                    },
                    Some(SchedEvent {
                        dettid: next_tid, ..
                    }),
                    _,
                ) if tid != next_tid => {
                    trace!(
                        "branch count to set before a context switch event: {}",
                        count
                    );
                    Some(count)
                }

                _ => None,
            }
        {
            return Some(LogicalTime::from_rcbs(*next_time_slice_in_brances as u64));
        }
        None
    }

    fn pop_next(&mut self) -> Option<SchedEvent> {
        self.cursor.next()
    }

    fn peek_next(&self) -> Option<&SchedEvent> {
        self.cursor.peek()
    }

    pub fn observe_event(&mut self, observed: &SchedEvent) -> ReplayAction {
        let mytid = observed.dettid;
        let time = observed.end_time.expect("timestamps required for now");
        let current_ix = self.traced_event_count;
        self.traced_event_count += 1;

        let next = self.pop_next();

        if let Some(expected) = next {
            debug!(
                "[detcore, dtid {}] {}: Ran event #{} {:?}, current replay event: {:?}",
                mytid,
                compare_desync(observed, &expected),
                current_ix,
                observed,
                expected,
            );
            trace!(
                "[detcore, dtid {}] NEXT event to replay {:?}",
                mytid,
                self.peek_next()
            );

            if is_desync(observed, &expected) {
                let counts = self.desync_counts.entry(mytid).or_insert((0, 0));
                if is_hard_desync(observed, &expected) {
                    if self.die_on_desync {
                        eprintln!("Replay mode desynchronized from trace, bailing out.");
                        return ReplayAction::Stop(StopReason::FatalDesync);
                    }
                    counts.1 += 1;
                } else {
                    counts.0 += 1;
                }
            }

            if let Some(next_ev) = self.peek_next() {
                let is_prehook = matches!(observed.op, Op::Syscall(_, SyscallPhase::Prehook));

                let next_tid = next_ev.dettid;
                // Checking for an optional timeslice in case of RCB
                let timeslice = self.resolve_time_to_run(next_ev);

                if next_tid != observed.dettid {
                    if is_prehook {
                        info!(
                            "[detcore, dtid {}] CONTEXT SWITCH to {} after this syscall blocks.  Reprioritizing at time {}",
                            &mytid, next_tid, time
                        );
                    } else {
                        info!(
                            "[detcore, dtid {}] CONTEXT SWITCH to {} after the last event retired.  Reprioritizing at time {}",
                            &mytid, next_tid, time
                        );
                    }

                    // The *downgrading* of the current thread will be handled by the caller if the
                    // context switch is *now*.  If the last traced event on this thread is instead
                    // a prehook, well we don't deschedule the current thread quite yet.  Rather, we
                    // let the thread plow ahead, and actually block on the syscall, which will have
                    // the effect of descheduling the therad anyway.  After that, the priorities
                    // be set so as to make sure the correct thread (next_tid) runs.
                    return ReplayAction::ContextSwitch(!is_prehook, next_tid, timeslice);
                } else {
                    // We're still running the next event.
                    return ReplayAction::Continue(timeslice);
                }
            } else {
                return ReplayAction::Continue(None);
            }
        } else {
            if self.replay_exhausted_panic {
                eprintln!(
                    "[detcore, dtid {}] Replay trace ran out, stopping at unknown event {:?}",
                    &mytid, observed
                );
                return ReplayAction::Stop(StopReason::ReplayExausted);
            }
            info!(
                "[detcore, dtid {}] Replay trace ran out, unknown event {:?}",
                &mytid, observed
            );
        }
        ReplayAction::Continue(None)
    }
}

#[cfg(test)]
mod builder {
    use detcore_model::pid::DetTid;
    use detcore_model::schedule::InstructionPointer;
    use detcore_model::schedule::Op;
    use detcore_model::schedule::SchedEvent;
    use detcore_model::schedule::SyscallPhase;
    use detcore_model::time::DetTime;
    use reverie::syscalls::Sysno;

    pub struct SchedTestBuilder {
        time: DetTime,
    }

    impl SchedTestBuilder {
        pub fn syscall_pre(&mut self, sys_no: Sysno, end_rip: usize, tid: DetTid) -> SchedEvent {
            SchedEvent {
                count: 1,
                dettid: tid,
                start_rip: None,
                end_rip: InstructionPointer::new(end_rip),
                end_time: Some(self.time.as_nanos()),
                op: Op::Syscall(sys_no, SyscallPhase::Prehook),
            }
        }

        pub fn syscall_post(&mut self, sys_no: Sysno, end_rip: usize, tid: DetTid) -> SchedEvent {
            self.time.add_syscall();
            SchedEvent {
                count: 1,
                dettid: tid,
                start_rip: None,
                end_rip: InstructionPointer::new(end_rip),
                end_time: Some(self.time.as_nanos()),
                op: Op::Syscall(sys_no, SyscallPhase::Posthook),
            }
        }

        pub fn other(&mut self, end_rip: usize, tid: DetTid) -> SchedEvent {
            SchedEvent {
                count: 1,
                dettid: tid,
                start_rip: None,
                end_time: Some(self.time.as_nanos()),
                op: Op::OtherInstructions,
                end_rip: InstructionPointer::new(end_rip),
            }
        }

        pub fn branch(&mut self, count: u32, tid: DetTid) -> SchedEvent {
            self.time.add_rcbs(count as u64);
            SchedEvent {
                count,
                dettid: tid,
                end_rip: None,
                end_time: Some(self.time.as_nanos()),
                op: Op::Branch,
                start_rip: None,
            }
        }

        pub fn new() -> Self {
            Self {
                time: Default::default(),
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use std::iter::FromIterator;

    use pretty_assertions::assert_eq;
    use reverie::syscalls::Sysno;

    use super::*;

    const SOME_RIP: usize = 1234566;

    fn tid(raw: i32) -> DetTid {
        DetTid::from_raw(raw)
    }

    fn create_replayer(i: impl IntoIterator<Item = SchedEvent>) -> Replayer {
        Replayer {
            cursor: ReplayCursor::from_iter(i),
            desync_counts: BTreeMap::new(),
            die_on_desync: false,
            replay_exhausted_panic: false,
            traced_event_count: 0,
        }
    }

    #[test]
    fn test_continue_() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![b.branch(10, tid(3))];
        let mut replayer = create_replayer(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(None)
        );
    }

    #[test]
    fn test_respect_branch_count() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(3)), //current instruction
            b.branch(123, tid(3)),     // next to respect branches
            b.other(SOME_RIP, tid(3)),
            b.branch(1, tid(5)),
        ];
        let mut replayer = create_replayer(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(Some(LogicalTime::from_rcbs(123)))
        );
    }

    #[test]
    fn test_respect_branch_context_switch() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(3)), //current instruction
            b.branch(123, tid(3)),     // next to respect branches
            b.other(SOME_RIP, tid(3)),
            b.syscall_post(Sysno::exit, SOME_RIP, tid(5)),
        ];
        let mut replayer = create_replayer(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(Some(LogicalTime::from_rcbs(123)))
        );
    }

    #[test]
    fn test_context_switch_pre_hook() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(5)),
            b.syscall_pre(Sysno::exit, SOME_RIP, tid(3)),
        ];
        let mut replayer = create_replayer(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::ContextSwitch(true, tid(3), None)
        );
    }

    #[test]
    fn test_context_switch_post_hook() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(5)),
            b.syscall_post(Sysno::exit, SOME_RIP, tid(3)),
        ];
        let mut replayer = create_replayer(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::ContextSwitch(true, tid(3), None)
        );
    }
}
