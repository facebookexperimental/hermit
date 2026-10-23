/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Replay cursor for the explicitly versioned stream. Legacy replay keeps its
//! own cursor and recovery behavior. A protocol error never advances this
//! cursor, and there is no fast-forward across a boundary.

use detcore_model::pid::DetTid;
use detcore_model::schedule::Op;
use detcore_model::schedule::SchedEvent;
use detcore_model::schedule::SyscallPhase;
use detcore_model::schedule::boundary::Error;
use detcore_model::schedule::boundary::Owner;
use detcore_model::schedule::boundary::Record;
use detcore_model::schedule::boundary::Schedule;

use super::replayer::ReplayAction;
use crate::slice_boundary::grant::Budget;

#[derive(Debug)]
pub(super) struct BoundaryReplayer {
    schedule: Schedule,
    next: usize,
    // A partial Branch stays at the same stored index. End RIP/time belong to
    // the complete span, so they are checked only when that span is complete.
    branch_consumed: u32,
}

impl BoundaryReplayer {
    pub(super) fn new(schedule: Schedule) -> Self {
        Self {
            schedule,
            next: 0,
            branch_consumed: 0,
        }
    }

    pub(super) fn stored_index(&self) -> usize {
        self.next
    }

    pub(super) fn has_records_left(&self) -> bool {
        self.next < self.schedule.records().len()
    }

    pub(super) fn next_thread(&self) -> Option<DetTid> {
        self.schedule.records().get(self.next).map(record_thread)
    }

    /// Look ahead within the next owner's contiguous segment. A future exec
    /// incarnation's expectation cannot be delivered to the old live owner.
    /// The caller passes its authenticated current owner, never one copied
    /// from this recording. A later grant after exec can ask again.
    pub(super) fn budget_for(&self, owner: Owner) -> Option<Budget> {
        for (index, record) in self.schedule.records().iter().enumerate().skip(self.next) {
            if record_thread(record) != owner.tid {
                break;
            }
            if let Record::TimesliceEnd(boundary) = record {
                return (boundary.identity.owner == owner)
                    .then(|| Budget::at(&self.schedule, index).expect("validated boundary index"));
            }
        }
        None
    }

    #[cfg(test)]
    pub(super) fn record(&self, index: usize) -> Option<&Record> {
        self.schedule.records().get(index)
    }

    pub(super) fn finish(&self) -> Result<(), Error> {
        if self.has_records_left() {
            Err(Error::Incomplete {
                index: self.next,
                remaining: self.schedule.records().len() - self.next,
            })
        } else {
            Ok(())
        }
    }

    pub(super) fn observe(&mut self, observed: &Record) -> Result<ReplayAction, Error> {
        let Some(expected) = self.schedule.records().get(self.next) else {
            return Err(Error::Exhausted {
                index: self.next,
                observed: Box::new(observed.clone()),
            });
        };
        let mut remaining_partial = None;
        if let (Record::Event(expected_event), Record::Event(observed_event)) = (expected, observed)
            && expected_event.op == Op::Branch
        {
            let remaining = expected_event.count - self.branch_consumed;
            let same_span = expected_event.dettid == observed_event.dettid
                && observed_event.op == Op::Branch
                && observed_event.count > 0
                && observed_event.count <= remaining;
            let starts_match = self.branch_consumed != 0
                || expected_event.start_rip.is_none()
                || expected_event.start_rip == observed_event.start_rip;
            if !same_span || !starts_match {
                return Err(self.mismatch(expected, observed));
            }
            if observed_event.count < remaining {
                remaining_partial = Some(self.branch_consumed + observed_event.count);
            } else {
                // The final suffix must retain the stored endpoint evidence.
                // Its own starting instruction is inside the span if an
                // earlier prefix already retired, not the span's first RIP.
                let mut suffix = expected_event.clone();
                suffix.count = remaining;
                if self.branch_consumed != 0 {
                    suffix.start_rip = None;
                }
                Record::Event(suffix).check_observation(self.next, observed)?;
            }
        } else {
            expected.check_observation(self.next, observed)?;
        }
        // Mutation is after every check above. A malformed boundary cannot
        // skip a partial Branch or change the partial offset.
        if let Some(consumed) = remaining_partial {
            self.branch_consumed = consumed;
            return Ok(ReplayAction::Continue(None));
        }
        self.branch_consumed = 0;
        self.next += 1;
        let Some(next_tid) = self.next_thread() else {
            return Ok(ReplayAction::Continue(None));
        };
        let current_tid = record_thread(observed);
        if next_tid == current_tid {
            return Ok(ReplayAction::Continue(None));
        }
        // A marker belongs immediately before its original resource request.
        // That request must do the handoff. Asking TraceReplay to yield here
        // would add a scheduler turn. Ordinary prehooks keep their existing
        // deferred switch semantics as well.
        let deferred = matches!(observed, Record::TimesliceEnd(_))
            || matches!(
                observed,
                Record::Event(SchedEvent {
                    op: Op::Syscall(_, SyscallPhase::Prehook),
                    ..
                })
            );
        Ok(ReplayAction::ContextSwitch(!deferred, next_tid, None))
    }

    fn mismatch(&self, expected: &Record, observed: &Record) -> Error {
        Error::Mismatch {
            index: self.next,
            expected: Box::new(expected.clone()),
            observed: Box::new(observed.clone()),
        }
    }
}

fn record_thread(record: &Record) -> DetTid {
    match record {
        Record::Event(event) => event.dettid,
        Record::TimesliceEnd(boundary) => boundary.identity.owner.tid,
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::schedule::InstructionPointer;
    use detcore_model::schedule::boundary::Boundary;
    use detcore_model::schedule::boundary::Cause;
    use detcore_model::schedule::boundary::Entry;
    use detcore_model::schedule::boundary::Identity;
    use detcore_model::schedule::boundary::Phase;
    use detcore_model::time::LogicalTime;
    use reverie::syscalls::Sysno;

    use super::*;

    fn owner(tid: i32) -> Owner {
        Owner {
            tid: DetTid::from_raw(tid),
            thread_generation: 0,
            birth_tid: DetTid::from_raw(tid),
            birth_generation: 0,
            address_space_creator: DetTid::from_raw(tid),
            address_space_generation: 0,
        }
    }
    fn boundary(tid: i32, slice: u64, rcbs: u64, cause: Cause) -> Record {
        Record::TimesliceEnd(Boundary {
            identity: Identity {
                owner: owner(tid),
                cause,
                phase: Phase::Prehook,
                entry: if cause == Cause::PmuTimer {
                    Entry::PmuTimer
                } else {
                    Entry::Syscall {
                        syscall: Sysno::getpid,
                        occurrence: 1,
                    }
                },
                committed_rcbs: rcbs,
                checkin: 1,
                slice,
            },
            count: 1,
            start_rip: None,
            end_rip: InstructionPointer::new(123),
            end_time: Some(LogicalTime::from_nanos(20)),
        })
    }
    fn event(tid: i32, op: Op, count: u32) -> Record {
        Record::Event(SchedEvent {
            dettid: DetTid::from_raw(tid),
            op,
            count,
            start_rip: None,
            end_rip: None,
            end_time: None,
        })
    }
    fn replay(records: Vec<Record>) -> BoundaryReplayer {
        BoundaryReplayer::new(Schedule::new(records).unwrap())
    }

    #[test]
    fn boundary_switch_uses_original_request_and_indices_include_markers() {
        let a = boundary(3, 1, 91, Cause::SoftwareCheckin);
        let b = boundary(4, 1, 0, Cause::PmuTimer);
        let mut r = replay(vec![a.clone(), b.clone()]);
        assert_eq!(r.budget_for(owner(3)).unwrap().stored_index(), 0);
        assert_eq!(
            r.observe(&a).unwrap(),
            ReplayAction::ContextSwitch(false, owner(4).tid, None)
        );
        assert_eq!(r.stored_index(), 1);
        assert_eq!(r.record(1), Some(&b));
        assert_eq!(r.budget_for(owner(4)).unwrap().stored_index(), 1);
        assert_eq!(r.observe(&b).unwrap(), ReplayAction::Continue(None));
        r.finish().unwrap();
    }

    #[test]
    fn lookahead_uses_cause_not_rip_and_does_not_cross_another_thread_or_exec_owner() {
        for cause in [Cause::SoftwareCheckin, Cause::PmuTimer] {
            let marker = boundary(3, 1, 91, cause);
            let mut r = replay(vec![event(3, Op::Branch, 91), marker.clone()]);
            let budget = r.budget_for(owner(3)).unwrap();
            assert_eq!(budget.expected().cause, cause);
            assert_eq!(budget.expected().committed_rcbs, 91);
            assert_eq!(budget.stored_index(), 1);
            assert!(r.budget_for(owner(4)).is_none());
            r.observe(&event(3, Op::Branch, 91)).unwrap();
            assert_eq!(r.budget_for(owner(3)), Some(budget));
        }
        let mut next_owner = owner(3);
        next_owner.address_space_generation = 1;
        let mut marker = boundary(3, 1, 0, Cause::PmuTimer);
        if let Record::TimesliceEnd(b) = &mut marker {
            b.identity.owner = next_owner;
        }
        let r = replay(vec![marker]);
        assert!(r.budget_for(owner(3)).is_none());
        assert_eq!(
            r.budget_for(next_owner).unwrap().expected().owner,
            next_owner
        );
        let r = replay(vec![
            event(4, Op::OtherInstructions, 1),
            boundary(3, 1, 91, Cause::PmuTimer),
        ]);
        assert!(r.budget_for(owner(3)).is_none());
    }

    #[test]
    fn repeated_zero_rcb_boundaries_remain_distinct_and_failure_never_advances() {
        let a = boundary(3, 1, 0, Cause::SoftwareCheckin);
        let b = boundary(3, 2, 0, Cause::SoftwareCheckin);
        let mut r = replay(vec![a.clone(), b.clone()]);
        assert!(r.observe(&b).is_err());
        assert_eq!(r.stored_index(), 0);
        r.observe(&a).unwrap();
        assert_eq!(r.stored_index(), 1);
        assert!(r.observe(&a).is_err());
        assert_eq!(r.stored_index(), 1);
        r.observe(&b).unwrap();
        r.finish().unwrap();
        assert!(r.observe(&b).is_err());
        assert_eq!(r.stored_index(), 2);
    }

    #[test]
    fn every_marker_identity_and_endpoint_is_strict_even_when_ordinary_events_would_resync() {
        let marker = boundary(3, 1, 91, Cause::SoftwareCheckin);
        for field in 0..12 {
            let mut bad = marker.clone();
            if let Record::TimesliceEnd(b) = &mut bad {
                match field {
                    0 => b.identity.owner.tid = owner(4).tid,
                    1 => b.identity.owner.thread_generation += 1,
                    2 => b.identity.owner.address_space_creator = owner(4).tid,
                    3 => b.identity.owner.address_space_generation += 1,
                    4 => b.identity.cause = Cause::PmuTimer,
                    5 => b.identity.phase = Phase::Posthook,
                    6 => b.identity.checkin += 1,
                    7 => b.identity.slice += 1,
                    8 => b.identity.committed_rcbs += 1,
                    9 => b.count += 1,
                    10 => b.end_rip = InstructionPointer::new(456),
                    11 => b.end_time = Some(LogicalTime::from_nanos(21)),
                    _ => unreachable!(),
                }
            }
            let mut r = replay(vec![marker.clone(), event(3, Op::OtherInstructions, 1)]);
            assert!(
                matches!(r.observe(&bad), Err(Error::Mismatch { index: 0, .. })),
                "field {field}"
            );
            assert_eq!(r.stored_index(), 0);
            assert!(r.finish().is_err());
        }
    }

    #[test]
    fn partial_branch_keeps_stored_index_and_requires_exact_final_endpoint() {
        let mut whole = match event(3, Op::Branch, 10) {
            Record::Event(e) => e,
            _ => unreachable!(),
        };
        whole.start_rip = InstructionPointer::new(100);
        whole.end_rip = InstructionPointer::new(200);
        whole.end_time = Some(LogicalTime::from_nanos(1000));
        let marker = boundary(3, 1, 10, Cause::SoftwareCheckin);
        let mut r = replay(vec![Record::Event(whole.clone()), marker.clone()]);
        let prefix = SchedEvent {
            count: 4,
            end_rip: InstructionPointer::new(150),
            end_time: Some(LogicalTime::from_nanos(400)),
            ..whole.clone()
        };
        r.observe(&Record::Event(prefix)).unwrap();
        assert_eq!(r.stored_index(), 0);
        assert!(r.finish().is_err());
        assert!(r.observe(&marker).is_err());
        assert_eq!(r.branch_consumed, 4);
        let suffix = SchedEvent {
            count: 6,
            start_rip: InstructionPointer::new(150),
            ..whole.clone()
        };
        for bad in [
            SchedEvent {
                count: 7,
                ..suffix.clone()
            },
            SchedEvent {
                count: 0,
                ..suffix.clone()
            },
            SchedEvent {
                end_rip: InstructionPointer::new(201),
                ..suffix.clone()
            },
            SchedEvent {
                end_time: Some(LogicalTime::from_nanos(1001)),
                ..suffix.clone()
            },
        ] {
            assert!(r.observe(&Record::Event(bad)).is_err());
            assert_eq!(r.branch_consumed, 4);
            assert_eq!(r.stored_index(), 0);
        }
        r.observe(&Record::Event(suffix)).unwrap();
        assert_eq!(r.stored_index(), 1);
        r.observe(&marker).unwrap();
        r.finish().unwrap();
    }

    #[test]
    fn ordinary_prehook_defers_but_posthook_handoff_is_immediate() {
        for (phase, immediate) in [
            (SyscallPhase::Prehook, false),
            (SyscallPhase::Posthook, true),
        ] {
            let a = event(3, Op::Syscall(Sysno::getpid, phase), 1);
            let mut r = replay(vec![a.clone(), event(4, Op::OtherInstructions, 1)]);
            assert_eq!(
                r.observe(&a).unwrap(),
                ReplayAction::ContextSwitch(immediate, owner(4).tid, None)
            );
        }
    }
}
