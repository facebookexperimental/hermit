/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::ops::Add;

use detcore_model::collections::ReplayCursor;
use detcore_model::pid::DetTid;
use detcore_model::schedule::Op;
use detcore_model::schedule::SchedEvent;
use detcore_model::schedule::SyscallPhase;
use detcore_model::time::LogicalTime;
use tracing::debug;
use tracing::info;
use tracing::trace;

const LOOKAHEAD_WINDOW: usize = 10;

/// Replayer
#[derive(Debug)]
pub struct Replayer {
    /// A cursor that holds our place in the global total order of events being replayed.
    pub cursor: ReplayCursor<SchedEvent>,
    /// Keep track of how many events we have observed during replay.  The current value is the
    /// event number of the NEXT event to replay.
    pub traced_event_count: u64,

    /// The number of events we've popped from the recording.
    pub events_popped: u64,

    /// Desync events observed per thread.  Counts different kinds of desyncs.
    pub desync_counts: BTreeMap<DetTid, DesyncStats>,
    /// A cached copy of the same (immutable) field in Config.
    pub replay_exhausted_panic: bool,
    /// A cached copy of the same (immutable) field in Config.
    pub die_on_desync: bool,
}

/// The count of desyncs for a particular thread.
#[derive(Debug, PartialEq, Eq, Default, Copy, Clone)]
pub struct DesyncStats {
    /// Not inclusive of hard desyncs.  You must add soft+hard together to get total desyncs.
    pub soft: u64,
    pub hard: u64,
    /// The very worst kind, where we need to context switch, but we don't have a match on the
    /// current event. These are a subset of the hard desyncs.
    pub at_context_switch: u64,
    /// How many times do we find a match again, after a mismatch, specifically new obserserved
    /// events that were not expected.
    pub resync_insertions: u64,
    /// How many times do we find a match again, after a mismatch, specifically expected events that
    /// were not observed.
    pub resync_deletions: u64,
    /// A bit of statefulness: true when the last event processed was a desync.
    pub last_was_desync: bool,
}

impl Add for DesyncStats {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        DesyncStats {
            soft: self.soft + rhs.soft,
            hard: self.hard + rhs.hard,
            at_context_switch: self.at_context_switch + rhs.at_context_switch,
            resync_insertions: self.resync_insertions + rhs.resync_insertions,
            resync_deletions: self.resync_deletions + rhs.resync_deletions,
            // Not commutative!
            last_was_desync: rhs.last_was_desync,
        }
    }
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
    /// The boolean indicates if the context switch should be immediate (or wait till the current
    /// thread blocks itself and naturally yields control).
    ContextSwitch(bool, DetTid, Option<LogicalTime>),
}

fn compare_desync(observed: &SchedEvent, expected: &SchedEvent) -> String {
    if !is_desync(observed, expected) {
        "SYNCED".to_string()
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

    if expected.start_rip.is_some() && observed.start_rip != expected.start_rip {
        return true;
    }

    if expected.end_rip.is_some() && observed.end_rip != expected.end_rip {
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
        self.events_popped += 1;
        self.cursor.next()
    }

    fn peek_next(&self) -> Option<SchedEvent> {
        self.cursor.peek().cloned()
    }

    fn next_replay_matches(&self, observed: &SchedEvent) -> bool {
        if let Some(expected) = self.cursor.peek() {
            !is_hard_desync(observed, expected)
        } else {
            false
        }
    }

    pub fn observe_event(&mut self, observed: &SchedEvent) -> ReplayAction {
        let mytid = observed.dettid;
        let current_ix = self.traced_event_count;
        self.traced_event_count += 1;

        if let Some(expected) = self.peek_next() {
            debug!(
                "[detcore, dtid {}] {}: Ran event #{} {}, current replay event: {}",
                mytid,
                compare_desync(observed, &expected),
                current_ix,
                observed,
                expected,
            );
            if let Some(nxt) = self.cursor.peek_nth(1) {
                trace!("[detcore, dtid {}] NEXT event to replay {}", mytid, nxt);
            }
            if observed.dettid != expected.dettid {
                tracing::warn!(
                    "[tracereplay] expected to be running thread {}, but running {} at event #{}",
                    expected.dettid,
                    observed.dettid,
                    current_ix
                );
            }

            let is_desync = is_desync(observed, &expected);
            let mut in_hard_desync_mode = false;
            if is_desync {
                let counts = self.desync_counts.entry(mytid).or_default();
                if is_hard_desync(observed, &expected) {
                    if self.die_on_desync {
                        eprintln!("Replay mode desynchronized from trace, bailing out.");
                        return ReplayAction::Stop(StopReason::FatalDesync);
                    }
                    counts.hard += 1;
                    in_hard_desync_mode = true;
                } else {
                    // TODO: we need to handle a mismatch in counts by doing a partial-pop on the
                    // replayer state.
                    counts.soft += 1;
                }
            }
            self.counts_next_round(mytid, is_desync);

            if is_desync {
                if let Some(ix) = self.check_fast_forward(observed) {
                    debug_assert!(ix > 0);
                    // This counts DELETIONS (missing events from observed stream):
                    let counts = self.desync_counts.entry(mytid).or_default();
                    counts.resync_deletions += 1;
                    in_hard_desync_mode = false;
                    // is_desync = false; // Logically correct, but... unused_assignments
                }
            }

            if !in_hard_desync_mode {
                // Whether naturally or by fast forward, we should end up in this state before we pop:
                if !self.next_replay_matches(observed) {
                    tracing::warn!(
                        "Expected match before pop, observed: {}\n Cursor at: {}",
                        observed,
                        self.cursor.display_first_n(10)
                    );
                }
                let _ = self.pop_next();
            }

            let action = self.check_context_switch(observed);
            if let ReplayAction::ContextSwitch(_, next_tid, _) = action {
                if in_hard_desync_mode {
                    tracing::warn!(
                        "[dtid {}] context switch to {}, but in a desynced state at this event (#{}) in --replay-schedule-from",
                        &mytid,
                        next_tid,
                        current_ix
                    );
                    let counts = self.desync_counts.entry(mytid).or_default();
                    counts.at_context_switch += 1;
                }
            }
            action
        } else if self.replay_exhausted_panic {
            eprintln!(
                "[detcore, dtid {}] Replay trace ran out, stopping at unknown event {:?}",
                &mytid, observed
            );
            ReplayAction::Stop(StopReason::ReplayExausted)
        } else {
            info!(
                "[detcore, dtid {}] Replay trace ran out, unknown event {:?}",
                &mytid, observed
            );
            ReplayAction::Continue(None)
        }
    }

    /// Logically advance the counting machinery to the next round.
    fn counts_next_round(&mut self, mytid: DetTid, is_desync: bool) {
        let counts = self.desync_counts.entry(mytid).or_default();
        if is_desync {
            // Set it for the next round:
            counts.last_was_desync = true;
        } else {
            if counts.last_was_desync {
                // This counts INSERTION events (new events in observed stream), because we
                // previously desynced and left unmatched events on the replay cursor, which are
                // now matched.
                counts.resync_insertions += 1;
            }
            counts.last_was_desync = false;
        }
    }

    /// Return how many to drop to have observed equal to the head of the replay tape,
    /// i.e. jump past missing or mismatched events on the replay tape.
    fn check_fast_forward(&mut self, observed: &SchedEvent) -> Option<usize> {
        if let Some(ix) = self.cursor.prefix_contains(LOOKAHEAD_WINDOW, observed) {
            // Drop the corresponding number of of missed/mismatched events so that the observed
            // matches the head of the replay tape (which is to be popped).
            self.cursor.drop(ix);
            Some(ix)
        } else {
            None
        }
    }

    fn check_context_switch(&mut self, observed: &SchedEvent) -> ReplayAction {
        // We run after the pop of any matched action, so the next action in replay represents the
        // FUTURE not the PRESENT/PAST:
        if let Some(next_ev) = self.cursor.peek() {
            let mytid = observed.dettid;
            let time = observed.end_time.expect("timestamps required for now");
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
                //
                // Note: for *natural* schedules that are recorded from hermit, the only reason
                // to switch away from the thread after the prehook is if it blocked. But a
                // synthetic schedule can choose to do this arbitrarily, and we need to assign
                // semantics to that on replay.  If we have threads A&B, and events "A_prehook,
                // B_*, A_posthook", then does the effect of A's syscall happen before or after
                // B's logic?
                ReplayAction::ContextSwitch(!is_prehook, next_tid, timeslice)
            } else {
                // We're still running the next event.
                ReplayAction::Continue(timeslice)
            }
        } else {
            ReplayAction::Continue(None)
        }
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
            events_popped: 0,
        }
    }

    fn strip_times(vec: Vec<SchedEvent>) -> Vec<SchedEvent> {
        vec.iter()
            .map(|se| SchedEvent {
                end_time: Some(LogicalTime::from_nanos(0)),
                ..*se
            })
            .collect()
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

    #[test]
    fn test_fast_forward() {
        let mut b = builder::SchedTestBuilder::new();

        // Strip times so we don't have a time mismatch in the branch 123 events:
        let observed = strip_times(vec![
            b.other(SOME_RIP, tid(3)),
            b.branch(123, tid(3)),
            b.syscall_pre(Sysno::exit, SOME_RIP, tid(3)),
        ]);
        let expected = strip_times(vec![
            b.branch(123, tid(3)),
            b.syscall_pre(Sysno::exit, SOME_RIP, tid(3)),
        ]);
        let mut replayer = create_replayer(expected);
        let action1 = replayer.observe_event(&observed[0]);
        {
            let counts = replayer.desync_counts.get(&tid(3)).unwrap();
            assert_eq!(
                *counts,
                DesyncStats {
                    soft: 0,
                    hard: 1,
                    at_context_switch: 0,
                    resync_insertions: 0,
                    resync_deletions: 0,
                    last_was_desync: true,
                }
            );
        }
        assert_eq!(action1, ReplayAction::Continue(None));

        let action2 = replayer.observe_event(&observed[1]);
        {
            let counts = replayer.desync_counts.get(&tid(3)).unwrap();
            assert_eq!(
                *counts,
                DesyncStats {
                    soft: 0,
                    hard: 1,
                    at_context_switch: 0,
                    resync_insertions: 1,
                    resync_deletions: 0,
                    last_was_desync: false,
                }
            );
        }
        assert_eq!(action2, ReplayAction::Continue(None));
    }
}
