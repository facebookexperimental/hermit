/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Typed transport over an existing scheduler grant. Lookahead authenticates
//! the live owner; the mailbox creates no new resource or turn.

use std::collections::BTreeMap;

use anyhow::bail;
use detcore_model::futex::MmId;
use detcore_model::pid::DetTid;
use detcore_model::schedule::boundary::Boundary;
use detcore_model::schedule::boundary::Cause;
use detcore_model::schedule::boundary::Identity;
use detcore_model::schedule::boundary::Owner;
use detcore_model::schedule::boundary::Record;
use detcore_model::schedule::boundary::Schedule;
use serde::Deserialize;
use serde::Serialize;

use crate::ThreadState;
use crate::config::Config;

/// An expectation from a validated stream, never an observed callback. An
/// absolute committed guest-RCB target does not include scheduler turn time.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Wire", into = "Wire")]
pub struct Budget {
    stored_index: usize,
    expected: Identity,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    stored_index: usize,
    expected: Identity,
}

impl TryFrom<Wire> for Budget {
    type Error = detcore_model::schedule::boundary::Error;

    fn try_from(wire: Wire) -> Result<Self, Self::Error> {
        Boundary {
            identity: wire.expected,
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: None,
        }
        .validate_shape(wire.stored_index)?;
        Ok(Self {
            stored_index: wire.stored_index,
            expected: wire.expected,
        })
    }
}

impl From<Budget> for Wire {
    fn from(budget: Budget) -> Self {
        Self {
            stored_index: budget.stored_index,
            expected: budget.expected,
        }
    }
}

impl Budget {
    /// Select a boundary in an already validated, single stored stream.
    pub fn at(schedule: &Schedule, index: usize) -> anyhow::Result<Self> {
        let Some(Record::TimesliceEnd(boundary)) = schedule.records().get(index) else {
            bail!("slice-boundary protocol refusal: budget index {index} is not a boundary");
        };
        Ok(Self {
            stored_index: index,
            expected: boundary.identity,
        })
    }

    /// Retain the complete mandatory identity for matching; no field here may
    /// be copied into the independently produced observation counters.
    pub fn expected(self) -> Identity {
        self.expected
    }

    /// Index in the complete stored stream, including boundary records.
    pub fn stored_index(self) -> usize {
        self.stored_index
    }
}

/// A pending expectation authenticated to the actual address space. It is
/// transported only when the existing RPC actually receives a grant. Control
/// replies cannot steal it, and terminal replies discard it without delivery.
#[derive(Default, Debug)]
pub struct Mailbox {
    pending: BTreeMap<DetTid, (MmId, Budget, bool)>,
}

impl Mailbox {
    /// Discard only a retired image's advertisement. A delayed old RPC must
    /// not consume a replacement image's pending budget on the same TID.
    pub(crate) fn retire(&mut self, tid: DetTid, mm: MmId) {
        if self
            .pending
            .get(&tid)
            .is_some_and(|(owner_mm, _, _)| *owner_mm == mm)
        {
            self.pending.remove(&tid);
        }
    }

    /// Stage a validated lookahead for an authenticated live owner. The
    /// scheduler integration must supply its registered MmId, not an expected
    /// trace's invented address space. Repeating the identical advertisement
    /// keeps its commit state; a different outstanding hint is refused.
    pub fn queue(&mut self, mm: MmId, budget: Budget) -> anyhow::Result<()> {
        let owner = budget.expected.owner;
        if owner.address_space_generation != mm.generation()
            || owner.address_space_creator != mm.creator()
        {
            bail!("slice-boundary protocol refusal: budget address-space generation mismatch");
        }
        if let Some((prior_mm, prior, _)) = self.pending.get(&owner.tid) {
            if *prior_mm == mm && *prior == budget {
                return Ok(());
            }
            bail!("slice-boundary protocol refusal: duplicate outstanding grant budget");
        }
        self.pending.insert(owner.tid, (mm, budget, false));
        Ok(())
    }

    /// Called only at the existing committed `unblock_guest` point. The
    /// broader RPC `Granted` marker also covers parked signal observation,
    /// which alone must not consume a pending boundary budget.
    pub fn committed_grant(&mut self, tid: DetTid) {
        if let Some((_, _, committed)) = self.pending.get_mut(&tid) {
            *committed = true;
        }
    }

    /// Finish the actual RPC, without modifying its status, resources, clock
    /// or run queue. Refusal retains the pending hint for diagnosis.
    pub fn finish(
        &mut self,
        tid: DetTid,
        mm: MmId,
        granted: bool,
        terminal: bool,
    ) -> anyhow::Result<Option<Budget>> {
        if terminal {
            self.retire(tid, mm);
            return Ok(None);
        }
        let Some((owner_mm, budget, committed)) = self.pending.get(&tid) else {
            return Ok(None);
        };
        if !granted || !committed {
            return Ok(None);
        }
        if *owner_mm != mm {
            bail!("slice-boundary protocol refusal: stale grant budget address space");
        }
        let budget = *budget;
        self.pending.remove(&tid);
        Ok(Some(budget))
    }
}

/// Apply a returned expectation after the completed slice's reset. This is
/// not timer/callback integration: software identities remain expectations,
/// and only a recorded PMU cause installs a precise PMU branch target. The
/// configured maximum and existing logical deadline are never changed here.
pub fn install<T>(thread: &mut ThreadState<T>, budget: Budget) -> anyhow::Result<()> {
    if thread
        .slice_boundary
        .as_ref()
        .is_some_and(|state| state.pending_start_budget.is_some())
    {
        bail!(
            "slice-boundary protocol refusal: ordinary grant cannot consume a staged start budget"
        );
    }
    install_checked(thread, budget)
}

fn install_checked<T>(thread: &mut ThreadState<T>, budget: Budget) -> anyhow::Result<()> {
    let budget = Budget::try_from(Wire {
        stored_index: budget.stored_index,
        expected: budget.expected,
    })?;
    let Some(observed) = &thread.slice_boundary else {
        bail!("slice-boundary protocol refusal: grant to an uninitialized boundary owner");
    };
    observed.require_owner(thread)?;
    let next_slice = observed.completed_slices.checked_add(1).ok_or_else(|| {
        anyhow::anyhow!("slice-boundary protocol refusal: slice occurrence overflow")
    })?;
    if budget.expected.owner != observed.owner
        || budget.expected.slice != next_slice
        || budget.expected.checkin < observed.checkin
        || budget.expected.committed_rcbs < thread.committed_clock_value
    {
        bail!("slice-boundary protocol refusal: stale or wrong-owner grant budget");
    }
    let precise =
        (budget.expected.cause == Cause::PmuTimer).then_some(budget.expected.committed_rcbs);
    if observed.expected_budget == Some(budget) && thread.replay_rcb_end == precise {
        return Ok(());
    }
    if observed.expected_budget.is_some() || thread.replay_rcb_end.is_some() {
        bail!("slice-boundary protocol refusal: grant would overwrite an active replay budget");
    }
    thread.slice_boundary.as_mut().unwrap().expected_budget = Some(budget);
    thread.replay_rcb_end = precise;
    Ok(())
}

/// A StartNewThread reply precedes its existing first reset. Keep the budget
/// pending, without arming a timer which that reset would immediately erase.
pub fn stage_start<T>(thread: &mut ThreadState<T>, budget: Budget) -> anyhow::Result<()> {
    let Some(observed) = &thread.slice_boundary else {
        bail!("slice-boundary protocol refusal: start grant has no authenticated owner");
    };
    observed.require_owner(thread)?;
    if observed.owner != budget.expected.owner
        || observed.checkin != 0
        || observed.completed_slices != 0
        || observed.current.is_some()
        || observed.expected_budget.is_some()
        || observed.pending_start_budget.is_some()
        || observed.start_reset_done
        || thread.replay_rcb_end.is_some()
        || budget.expected.slice != 1
        || budget.expected.committed_rcbs < thread.committed_clock_value
    {
        bail!("slice-boundary protocol refusal: invalid or repeated start budget");
    }
    thread.slice_boundary.as_mut().unwrap().pending_start_budget =
        Some((budget, thread.stats.timeslice_count));
    Ok(())
}

/// A one-use receipt for the original startup `next_timeslice` call. The
/// timer-disabled path intentionally does not increment timeslice statistics;
/// possession of this private receipt, not an inferred counter change, proves
/// that the call happened. It cannot be cloned or constructed by callers.
#[derive(Debug)]
pub(crate) struct StartReset {
    tid: DetTid,
    mm: MmId,
    owner: Option<Owner>,
    before: u64,
    after: u64,
}

/// Perform exactly the startup reset already required by the caller. Preserve
/// its timer-disabled behavior and return evidence for installation after the
/// unchanged deadline-cap call. This does not add a scheduler request.
pub(crate) fn reset_start<T>(
    thread: &mut ThreadState<T>,
    cfg: &Config,
) -> anyhow::Result<StartReset> {
    let before = thread.stats.timeslice_count;
    let owner = if let Some(observed) = &thread.slice_boundary {
        observed.require_owner(thread)?;
        if observed.start_reset_done
            || observed.checkin != 0
            || observed.completed_slices != 0
            || observed.current.is_some()
            || observed
                .pending_start_budget
                .is_some_and(|(_, staged_before)| staged_before != before)
        {
            bail!("slice-boundary protocol refusal: repeated or late startup reset");
        }
        Some(observed.owner)
    } else {
        None
    };
    thread.next_timeslice(cfg);
    let after = thread.stats.timeslice_count;
    let increment = u64::from(cfg.target_timeslice.or(cfg.max_timeslice).is_some());
    if before.checked_add(increment) != Some(after) {
        bail!("slice-boundary protocol refusal: startup reset changed unexpected statistics");
    }
    if let Some(observed) = &mut thread.slice_boundary {
        observed.start_reset_done = true;
    }
    Ok(StartReset {
        tid: thread.dettid,
        mm: thread.mm_id,
        owner,
        before,
        after,
    })
}

/// Install the start grant after consuming proof of the actual startup reset,
/// and after its unchanged deadline-cap call. This never resets a slice or
/// advances clocks itself. Missing, stale and repeated resets are refused.
pub(crate) fn finish_start<T>(
    thread: &mut ThreadState<T>,
    reset: StartReset,
) -> anyhow::Result<()> {
    if reset.tid != thread.dettid
        || reset.mm != thread.mm_id
        || reset.after != thread.stats.timeslice_count
        || reset.owner != thread.slice_boundary.as_ref().map(|state| state.owner)
        || thread
            .slice_boundary
            .as_ref()
            .is_some_and(|state| !state.start_reset_done)
    {
        bail!("slice-boundary protocol refusal: missing or stale startup reset receipt");
    }
    let Some((budget, before)) = thread
        .slice_boundary
        .as_ref()
        .and_then(|state| state.pending_start_budget)
    else {
        return Ok(());
    };
    if before != reset.before {
        bail!("slice-boundary protocol refusal: start budget requires exactly one reset");
    }
    install_checked(thread, budget)?;
    thread.slice_boundary.as_mut().unwrap().pending_start_budget = None;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use detcore_model::schedule::boundary::Boundary;
    use detcore_model::schedule::boundary::Entry;
    use detcore_model::schedule::boundary::Owner;
    use detcore_model::schedule::boundary::Phase;

    use super::*;
    use crate::config::Config;
    use crate::slice_boundary::Observations;

    #[test]
    fn boundary_terminal_old_image_cannot_consume_replacement_budget() {
        let tid = DetTid::from_raw(3);
        let old = MmId::initial(tid);
        let new = old.for_exec(tid);
        let hint = budget(tid, new, 7);
        let mut mailbox = Mailbox::default();
        mailbox.queue(new, hint).unwrap();
        mailbox.committed_grant(tid);
        assert_eq!(mailbox.finish(tid, old, false, true).unwrap(), None);
        assert_eq!(mailbox.finish(tid, new, true, false).unwrap(), Some(hint));
    }

    pub(crate) fn budget(tid: DetTid, mm: MmId, rcbs: u64) -> Budget {
        let schedule = Schedule::new(vec![Record::TimesliceEnd(Boundary {
            identity: Identity {
                owner: Owner {
                    tid,
                    thread_generation: 0,
                    birth_tid: tid,
                    birth_generation: 0,
                    address_space_creator: mm.creator(),
                    address_space_generation: mm.generation(),
                },
                cause: Cause::PmuTimer,
                phase: Phase::Prehook,
                entry: Entry::PmuTimer,
                committed_rcbs: rcbs,
                checkin: 1,
                slice: 1,
            },
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: None,
        })])
        .unwrap();
        Budget::at(&schedule, 0).unwrap()
    }

    #[test]
    fn control_terminal_stale_exec_and_duplicate_grants_cannot_lose_or_reuse_a_budget() {
        let tid = DetTid::from_raw(3);
        let mm = MmId::initial(tid);
        let expected = budget(tid, mm, 19);
        let mut pending = Mailbox::default();
        pending.queue(mm, expected).unwrap();
        pending.queue(mm, expected).unwrap();
        assert!(pending.queue(mm, budget(tid, mm, 20)).is_err());
        assert_eq!(pending.finish(tid, mm, false, false).unwrap(), None);
        // A parked signal observation is marked Granted but has not committed
        // the next resource turn. It cannot consume this expectation.
        assert_eq!(pending.finish(tid, mm, true, false).unwrap(), None);
        pending.committed_grant(tid);
        assert!(pending.finish(tid, mm.for_exec(tid), true, false).is_err());
        assert_eq!(
            pending.finish(tid, mm, true, false).unwrap(),
            Some(expected)
        );
        assert_eq!(pending.finish(tid, mm, true, false).unwrap(), None);
        pending.queue(mm, expected).unwrap();
        assert_eq!(pending.finish(tid, mm, false, true).unwrap(), None);
        assert_eq!(pending.finish(tid, mm, true, false).unwrap(), None);
    }

    #[test]
    fn grant_is_guest_rcbs_not_turn_time_and_never_changes_deadline_or_maximum() {
        let cfg = Config::default();
        let tid = DetTid::from_raw(3);
        let mut thread = ThreadState::new(tid, &cfg, ());
        thread.next_timeslice(&cfg);
        thread.slice_boundary = Some(Observations::new(&thread, 0));
        thread.committed_clock_value = 7;
        thread
            .thread_logical_time
            .advance_to(thread.thread_logical_time.as_nanos() + std::time::Duration::from_secs(5));
        let before = (
            thread.thread_logical_time.as_nanos(),
            thread.end_of_timeslice,
            thread.max_timeslice_end,
            thread.stats.timeslice_count,
        );
        let hint = budget(tid, thread.mm_id, 19);
        install(&mut thread, hint).unwrap();
        assert_eq!(thread.replay_rcb_end, Some(19));
        assert_eq!(thread.committed_clock_value, 7);
        assert_eq!(
            (
                thread.thread_logical_time.as_nanos(),
                thread.end_of_timeslice,
                thread.max_timeslice_end,
                thread.stats.timeslice_count
            ),
            before
        );
        install(&mut thread, hint).unwrap();
        let replacement = budget(tid, thread.mm_id, 20);
        assert!(install(&mut thread, replacement).is_err());
        assert_eq!(thread.replay_rcb_end, Some(19));
    }

    #[test]
    fn start_grant_survives_exactly_one_real_reset_and_refuses_other_lifetimes() {
        let tid = DetTid::from_raw(3);
        for timers in [true, false] {
            let cfg = Config {
                max_timeslice: if timers {
                    Config::default().max_timeslice
                } else {
                    None
                },
                ..Config::default()
            };
            for resets in [0, 1, 2] {
                let mut thread = ThreadState::new(tid, &cfg, ());
                thread.slice_boundary = Some(Observations::new(&thread, 0));
                let mut hint = budget(tid, thread.mm_id, 19);
                if !timers {
                    hint.expected.entry = Entry::Syscall {
                        syscall: reverie::syscalls::Sysno::sched_yield,
                        occurrence: 1,
                    };
                    hint.expected.cause = Cause::ExplicitSchedYield;
                }
                let clock = thread.thread_logical_time.as_nanos();
                stage_start(&mut thread, hint).unwrap();
                assert_eq!(thread.replay_rcb_end, None);
                assert!(stage_start(&mut thread, hint).is_err());
                assert!(install(&mut thread, hint).is_err());
                if resets == 0 {
                    // Only this child test module can construct an invalid
                    // receipt; production callers cannot bypass reset_start.
                    let forged = StartReset {
                        tid,
                        mm: thread.mm_id,
                        owner: Some(hint.expected.owner),
                        before: 0,
                        after: 0,
                    };
                    let before =
                        serde_json::to_vec(thread.slice_boundary.as_ref().unwrap()).unwrap();
                    assert!(finish_start(&mut thread, forged).is_err());
                    assert_eq!(
                        serde_json::to_vec(thread.slice_boundary.as_ref().unwrap()).unwrap(),
                        before
                    );
                } else {
                    let reset = reset_start(&mut thread, &cfg).unwrap();
                    if resets == 2 {
                        let before =
                            serde_json::to_vec(thread.slice_boundary.as_ref().unwrap()).unwrap();
                        assert!(reset_start(&mut thread, &cfg).is_err());
                        assert_eq!(
                            serde_json::to_vec(thread.slice_boundary.as_ref().unwrap()).unwrap(),
                            before
                        );
                    }
                    finish_start(&mut thread, reset).unwrap();
                    assert_eq!(thread.replay_rcb_end, timers.then_some(19));
                    assert_eq!(
                        thread.slice_boundary.as_ref().unwrap().expected_budget,
                        Some(hint)
                    );
                    assert!(
                        thread
                            .slice_boundary
                            .as_ref()
                            .unwrap()
                            .pending_start_budget
                            .is_none()
                    );
                    assert!(stage_start(&mut thread, hint).is_err());
                }
                assert_eq!(thread.thread_logical_time.as_nanos(), clock);
                assert_eq!(
                    thread.stats.timeslice_count,
                    u64::from(timers && resets > 0)
                );
            }
            for wrong_lifetime in 0..3 {
                let mut thread = ThreadState::new(tid, &cfg, ());
                thread.slice_boundary = Some(Observations::new(&thread, 0));
                let hint = budget(tid, thread.mm_id, 19);
                stage_start(&mut thread, hint).unwrap();
                let reset = reset_start(&mut thread, &cfg).unwrap();
                match wrong_lifetime {
                    0 => thread.mm_id = thread.mm_id.for_exec(tid),
                    1 => thread.dettid = DetTid::from_raw(17),
                    2 => thread.stats.timeslice_count += 1,
                    _ => unreachable!(),
                }
                assert!(finish_start(&mut thread, reset).is_err());
                assert_eq!(thread.replay_rcb_end, None);
            }
        }
    }

    #[test]
    fn malformed_wire_and_wrong_owner_budget_are_refused_before_state_changes() {
        let cfg = Config::default();
        let tid = DetTid::from_raw(3);
        let mut thread = ThreadState::new(tid, &cfg, ());
        thread.next_timeslice(&cfg);
        thread.slice_boundary = Some(Observations::new(&thread, 0));
        thread.committed_clock_value = 7;
        let hint = budget(tid, thread.mm_id, 19);
        let bytes = serde_json::to_vec(&hint).unwrap();
        assert_eq!(serde_json::from_slice::<Budget>(&bytes).unwrap(), hint);
        for bad in 0..7 {
            let mut altered = hint;
            match bad {
                0 => altered.expected.owner.tid = DetTid::from_raw(4),
                1 => altered.expected.owner.thread_generation += 1,
                2 => altered.expected.owner.address_space_creator = DetTid::from_raw(4),
                3 => altered.expected.owner.address_space_generation += 1,
                4 => altered.expected.slice += 1,
                5 => altered.expected.committed_rcbs = 6,
                6 => altered.expected.checkin = 0,
                _ => unreachable!(),
            }
            let before = serde_json::to_vec(thread.slice_boundary.as_ref().unwrap()).unwrap();
            assert!(install(&mut thread, altered).is_err());
            assert_eq!(
                serde_json::to_vec(thread.slice_boundary.as_ref().unwrap()).unwrap(),
                before
            );
            assert_eq!(thread.replay_rcb_end, None);
        }
        for bad in 0..4 {
            let mut wire = serde_json::to_value(hint).unwrap();
            match bad {
                0 => wire["expected"]["slice"] = 0.into(),
                1 => wire["expected"]["checkin"] = 0.into(),
                2 => wire["expected"]["entry"] = "Cpuid".into(),
                3 => wire["ignored"] = true.into(),
                _ => unreachable!(),
            }
            assert!(serde_json::from_value::<Budget>(wire).is_err());
        }
    }
}
