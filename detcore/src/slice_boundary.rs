/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Slice-boundary observation/reset API. Authenticated startup activates it
//! for a typed recording or replay. Actual callback observations stay separate
//! from expectations; reset adds no scheduler request.

use anyhow::bail;
use detcore_model::futex::MmId;
use detcore_model::schedule::InstructionPointer;
use detcore_model::schedule::boundary::Boundary;
use detcore_model::schedule::boundary::Cause;
use detcore_model::schedule::boundary::Entry;
use detcore_model::schedule::boundary::Identity;
use detcore_model::schedule::boundary::Owner;
use detcore_model::schedule::boundary::Phase;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::ThreadState;
use crate::config::Config;
use crate::scheduler::Priority;

/// Typed grant transport; the legacy logical-nanosecond scalar cannot carry
/// a guest-RCB target. The live owner authenticates every lookahead.
pub mod grant;

/// Authenticated logical-thread identity across creation, exit and exec.
pub(crate) mod lifecycle;

/// Tokens for actual semantic callback entries, retained across awaits.
pub(crate) mod callbacks;

/// Counters owned by one live logical thread/address-space generation.
/// Creation and exec must install a freshly authenticated owner; replay hints
/// cannot initialize or advance this state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Observations {
    owner: Owner,
    mm: MmId,
    checkin: u64,
    completed_slices: u64,
    syscall_occurrence: u64,
    signal_entry_occurrence: u64,
    active_syscall: Option<(Sysno, u64)>,
    current: Option<(Entry, Phase, u64)>,
    expected_budget: Option<grant::Budget>,
    pending_start_budget: Option<(grant::Budget, u64)>,
    start_reset_done: bool,
}

/// One actual reset, with the completed identity captured before it. The
/// caller must trace/validate this record immediately before its existing
/// resource request, and only then install a replay budget. This object does
/// not grant a turn or introduce a TraceReplay resource.
#[derive(Debug)]
pub struct ClosedSlice {
    /// Evidence of the completed slice, not the newly selected one.
    pub boundary: Boundary,
    /// The unmodified priority choice returned by `next_timeslice`.
    pub next_priority: Option<Priority>,
}

impl Observations {
    /// Construct counters from an authenticated scheduler lifecycle identity.
    /// This path never consumes an expected boundary to choose an owner.
    fn from_owner<T>(thread: &ThreadState<T>, owner: Owner) -> anyhow::Result<Self> {
        if owner.tid != thread.dettid
            || owner.birth_tid.as_raw() <= 0
            || owner.address_space_creator != thread.mm_id.creator()
            || owner.address_space_generation != thread.mm_id.generation()
        {
            bail!("slice-boundary protocol refusal: lifecycle reply does not name this image");
        }
        let mut state = Self::new(thread, owner.thread_generation);
        state.owner = owner;
        Ok(state)
    }

    /// Bind a fresh observation state to the actual thread and address-space
    /// generation. The thread-generation allocator remains a scheduler
    /// integration obligation; this API does not infer it from host timing.
    pub fn new<T>(thread: &ThreadState<T>, thread_generation: u64) -> Self {
        Self {
            owner: Owner {
                tid: thread.dettid,
                thread_generation,
                birth_tid: thread.dettid,
                birth_generation: thread_generation,
                address_space_creator: thread.mm_id.creator(),
                address_space_generation: thread.mm_id.generation(),
            },
            mm: thread.mm_id,
            checkin: 0,
            completed_slices: 0,
            syscall_occurrence: 0,
            signal_entry_occurrence: 0,
            active_syscall: None,
            current: None,
            expected_budget: None,
            pending_start_budget: None,
            start_reset_done: false,
        }
    }

    fn require_owner<T>(&self, thread: &ThreadState<T>) -> anyhow::Result<()> {
        if self.owner.tid != thread.dettid || self.mm != thread.mm_id {
            bail!(
                "slice-boundary protocol refusal: stale observation owner {:?}",
                self.owner
            );
        }
        Ok(())
    }

    fn publish<T>(
        &mut self,
        thread: &ThreadState<T>,
        entry: Entry,
        phase: Phase,
    ) -> anyhow::Result<()> {
        self.require_owner(thread)?;
        let checkin = self.checkin.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("slice-boundary protocol refusal: semantic check-in overflow")
        })?;
        if self
            .current
            .is_some_and(|(_, _, rcbs)| thread.committed_clock_value < rcbs)
        {
            bail!("slice-boundary protocol refusal: committed guest RCB count moved backwards");
        }
        self.checkin = checkin;
        self.current = Some((entry, phase, thread.committed_clock_value));
        Ok(())
    }

    /// Observe an actual syscall check-in. The prehook allocates its own
    /// occurrence; its posthook must name that same syscall. Hidden backend
    /// stops and injected-call retries must not invoke this method.
    pub fn syscall<T>(
        &mut self,
        thread: &ThreadState<T>,
        syscall: Sysno,
        phase: Phase,
    ) -> anyhow::Result<()> {
        let occurrence = match phase {
            Phase::Prehook => self.syscall_occurrence.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!("slice-boundary protocol refusal: syscall occurrence overflow")
            })?,
            Phase::Posthook => match self.active_syscall {
                Some((active, occurrence)) if active == syscall => occurrence,
                _ => {
                    bail!("slice-boundary protocol refusal: syscall posthook has no matching entry")
                }
            },
            Phase::TimerMaximum => bail!("slice-boundary protocol refusal: syscall at timer body"),
        };
        self.publish(
            thread,
            Entry::Syscall {
                syscall,
                occurrence,
            },
            phase,
        )?;
        match phase {
            Phase::Prehook => {
                self.syscall_occurrence = occurrence;
                self.active_syscall = Some((syscall, occurrence));
            }
            Phase::Posthook => self.active_syscall = None,
            Phase::TimerMaximum => unreachable!(),
        }
        Ok(())
    }

    /// Observe a non-syscall callback delivered to Detcore. Timer mechanisms
    /// must be distinguished by the actual callback, not RIP or expectations.
    /// Signal delivery identity is the already selected logical delivery.
    pub fn callback<T>(
        &mut self,
        thread: &ThreadState<T>,
        entry: Entry,
        phase: Phase,
    ) -> anyhow::Result<()> {
        match entry {
            Entry::Syscall { .. } => {
                bail!("slice-boundary protocol refusal: use syscall occurrence allocator")
            }
            Entry::Signal {
                signal, delivery, ..
            } if delivery == 0 || !(1..=64).contains(&signal.raw()) => {
                bail!("slice-boundary protocol refusal: invalid logical signal delivery");
            }
            _ => self.publish(thread, entry, phase),
        }
    }

    /// Snapshot the completed identity and reset the actual thread exactly
    /// once. No replay expectation is accepted as an observation. Refusals
    /// precede any reset, RNG draw, clock change, or occurrence advancement.
    pub fn close_slice<T>(
        &mut self,
        thread: &mut ThreadState<T>,
        config: &Config,
        explicit_yield: bool,
        rip: Option<InstructionPointer>,
    ) -> anyhow::Result<ClosedSlice> {
        let identity = self.current_identity(thread, explicit_yield)?;
        let boundary = Boundary {
            identity,
            count: 1,
            start_rip: None,
            end_rip: rip,
            end_time: Some(thread.thread_logical_time.as_nanos()),
        };
        if let Some(expected) = self.expected_budget
            && expected.expected() != boundary.identity
        {
            bail!(
                "slice-boundary protocol refusal at stored index {}: expected {:?}, observed {:?}",
                expected.stored_index(),
                expected.expected(),
                boundary.identity
            );
        }
        let next_priority = thread.next_timeslice(config);
        self.completed_slices = identity.slice;
        self.expected_budget = None;
        Ok(ClosedSlice {
            boundary,
            next_priority,
        })
    }

    fn current_identity<T>(
        &self,
        thread: &ThreadState<T>,
        explicit_yield: bool,
    ) -> anyhow::Result<Identity> {
        self.require_owner(thread)?;
        if thread.own_signal_turn_hold.is_some() {
            bail!("slice-boundary protocol refusal: boundary conflicts with own-signal turn hold");
        }
        let Some((entry, phase, rcbs)) = self.current else {
            bail!("slice-boundary protocol refusal: slice end has no semantic check-in");
        };
        if rcbs != thread.committed_clock_value {
            bail!("slice-boundary protocol refusal: guest progressed beyond the observed check-in");
        }
        let cause = if explicit_yield {
            if !matches!(
                entry,
                Entry::Syscall {
                    syscall: Sysno::sched_yield,
                    ..
                }
            ) {
                bail!("slice-boundary protocol refusal: explicit yield at a different syscall");
            }
            Cause::ExplicitSchedYield
        } else {
            match entry {
                Entry::PmuTimer => Cause::PmuTimer,
                Entry::NoRcbTimer => Cause::NoRcbTimer,
                _ => Cause::SoftwareCheckin,
            }
        };
        let slice = self.completed_slices.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("slice-boundary protocol refusal: slice occurrence overflow")
        })?;
        Ok(Identity {
            owner: self.owner,
            cause,
            phase,
            entry,
            committed_rcbs: rcbs,
            checkin: self.checkin,
            slice,
        })
    }

    /// Whether the next expected boundary belongs at this actual check-in.
    /// Expected fields select a point, never fill the observed identity.
    /// Explicit yield and no-RCB maximum bodies keep their existing callers.
    fn replay_expiration<T>(&self, thread: &ThreadState<T>) -> anyhow::Result<Option<bool>> {
        let Some(budget) = self.expected_budget else {
            return Ok(None);
        };
        self.require_owner(thread)?;
        let expected = budget.expected();
        if self.checkin > expected.checkin || thread.committed_clock_value > expected.committed_rcbs
        {
            bail!(
                "slice-boundary protocol refusal at stored index {}: passed expected {:?}; actual check-in {}, guest RCBs {}",
                budget.stored_index(),
                expected,
                self.checkin,
                thread.committed_clock_value
            );
        }
        if self.checkin < expected.checkin {
            return Ok(Some(false));
        }
        let observed = self.current_identity(thread, false)?;
        // This automatic hook cannot claim an explicit yield occurred. It
        // checks only its location; the sched_yield handler must later close
        // with its independently known explicit cause and the full identity.
        let mut location = expected;
        if expected.cause == Cause::ExplicitSchedYield {
            location.cause = Cause::SoftwareCheckin;
        }
        if observed != location {
            bail!(
                "slice-boundary protocol refusal at stored index {}: expected {:?}, observed {:?}",
                budget.stored_index(),
                expected,
                observed
            );
        }
        Ok(Some(expected.cause != Cause::ExplicitSchedYield))
    }
}

pub(crate) fn replay_expiration<T>(thread: &ThreadState<T>) -> anyhow::Result<Option<bool>> {
    match &thread.slice_boundary {
        Some(observed) => observed.replay_expiration(thread),
        None => Ok(None),
    }
}

pub(crate) fn no_rcb_maximum<T>(thread: &mut ThreadState<T>) -> anyhow::Result<()> {
    if let Some(mut observed) = thread.slice_boundary.take() {
        let result = observed.publish(thread, Entry::NoRcbTimer, Phase::TimerMaximum);
        thread.slice_boundary = Some(observed);
        result?;
    }
    Ok(())
}

/// Adopt the owner authenticated by the same existing startup/exec grant.
/// Repeated ordinary grants leave every counter and pending budget intact.
/// Exec invalidates only observation state, never ThreadState's clocks,
/// committed turns, reset statistics, PRNG or configured timer maximum.
pub(crate) fn adopt_owner<T>(
    thread: &mut ThreadState<T>,
    owner: Owner,
    starts_thread: bool,
) -> anyhow::Result<()> {
    let candidate = Observations::from_owner(thread, owner)?;
    if let Some(prior) = &thread.slice_boundary {
        if prior.owner == owner {
            return prior.require_owner(thread);
        }
        if (prior.owner.birth_tid, prior.owner.birth_generation)
            != (owner.birth_tid, owner.birth_generation)
            || prior.owner.address_space_generation.checked_add(1)
                != Some(owner.address_space_generation)
            || prior.mm == thread.mm_id
        {
            bail!("slice-boundary protocol refusal: lifecycle reply replaced a different thread");
        }
    } else if !starts_thread {
        bail!("slice-boundary protocol refusal: first owner did not arrive through startup");
    }
    thread.slice_boundary = Some(candidate);
    // A previous image's precise target cannot govern the replacement image.
    thread.replay_rcb_end = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::time::Duration;

    use detcore_model::schedule::boundary::Record;
    use detcore_model::schedule::boundary::Schedule;

    use super::*;
    use crate::types::DetTid;

    fn fixture() -> (Config, ThreadState<()>, Observations) {
        let cfg = Config {
            max_timeslice: NonZeroU64::new(200_000_000),
            ..Default::default()
        };
        let mut thread = ThreadState::new(DetTid::from_raw(3), &cfg, ());
        thread.next_timeslice(&cfg);
        let observations = Observations::new(&thread, 0);
        (cfg, thread, observations)
    }

    #[test]
    fn shared_and_copied_clone_address_spaces_are_distinct_in_the_stored_identity() {
        let cfg = Config::default();
        let parent = DetTid::from_raw(3);
        let child = DetTid::from_raw(4);
        let mut shared = ThreadState::new(child, &cfg, ());
        shared.mm_id = MmId::for_clone(MmId::initial(parent), child, true);
        let mut copied = ThreadState::new(child, &cfg, ());
        copied.mm_id = MmId::for_clone(MmId::initial(parent), child, false);
        let mut a = Observations::new(&shared, 0);
        let mut b = Observations::new(&copied, 0);
        a.callback(&shared, Entry::Cpuid, Phase::Prehook).unwrap();
        b.callback(&copied, Entry::Cpuid, Phase::Prehook).unwrap();
        let a = a
            .close_slice(&mut shared, &cfg, false, None)
            .unwrap()
            .boundary;
        let b = b
            .close_slice(&mut copied, &cfg, false, None)
            .unwrap()
            .boundary;
        assert_ne!(a.identity.owner, b.identity.owner);
        let schedule = Schedule::new(vec![Record::TimesliceEnd(a)]).unwrap();
        let mut cursor = schedule.cursor();
        assert!(cursor.observe(&Record::TimesliceEnd(b)).is_err());
        assert_eq!(cursor.stored_index(), 0);
    }

    #[test]
    fn actual_reset_snapshots_completed_slice_without_moving_guest_clock() {
        let (cfg, mut thread, mut observed) = fixture();
        thread.committed_clock_value = 71;
        thread.thread_logical_time.add_rcbs(71);
        thread.stats.count_syscall(Sysno::write);
        observed
            .syscall(&thread, Sysno::write, Phase::Prehook)
            .unwrap();
        let before_clock = thread.thread_logical_time.clone();
        let before_slices = thread.stats.timeslice_count;
        let closed = observed
            .close_slice(&mut thread, &cfg, false, None)
            .unwrap();
        assert_eq!(closed.boundary.identity.committed_rcbs, 71);
        assert_eq!(closed.boundary.identity.slice, 1);
        assert_eq!(closed.boundary.end_time, Some(before_clock.as_nanos()));
        assert_eq!(
            thread.thread_logical_time.as_nanos(),
            before_clock.as_nanos()
        );
        assert_eq!(thread.committed_clock_value, 71);
        assert_eq!(thread.stats.timeslice_count, before_slices + 1);
        assert_eq!(thread.stats.timeslice_syscall_count, 0);
        assert_eq!(
            thread.max_timeslice_end,
            Some(before_clock.as_nanos() + Duration::from_nanos(200_000_000))
        );
    }

    #[test]
    fn adjacent_zero_rcb_resets_and_repeated_syscalls_have_distinct_identities() {
        let (cfg, mut thread, mut observed) = fixture();
        observed
            .syscall(&thread, Sysno::write, Phase::Prehook)
            .unwrap();
        let first = observed
            .close_slice(&mut thread, &cfg, false, None)
            .unwrap()
            .boundary;
        let second = observed
            .close_slice(&mut thread, &cfg, false, None)
            .unwrap()
            .boundary;
        assert_eq!(first.identity.checkin, second.identity.checkin);
        assert_eq!(
            first.identity.committed_rcbs,
            second.identity.committed_rcbs
        );
        assert_ne!(first.identity.slice, second.identity.slice);
        observed
            .syscall(&thread, Sysno::write, Phase::Posthook)
            .unwrap();
        observed
            .syscall(&thread, Sysno::write, Phase::Prehook)
            .unwrap();
        let third = observed
            .close_slice(&mut thread, &cfg, false, None)
            .unwrap()
            .boundary;
        assert_ne!(first.identity.entry, third.identity.entry);
        Schedule::new(vec![
            Record::TimesliceEnd(first),
            Record::TimesliceEnd(second),
            Record::TimesliceEnd(third),
        ])
        .unwrap();
    }

    #[test]
    fn signal_hold_stale_exec_and_wrong_callback_refuse_before_reset() {
        for bad in 0..5 {
            let (cfg, mut thread, mut observed) = fixture();
            observed
                .syscall(&thread, Sysno::write, Phase::Prehook)
                .unwrap();
            match bad {
                0 => thread.own_signal_turn_hold = Some(libc::SIGALRM),
                1 => thread.mm_id = thread.mm_id.for_exec(thread.dettid),
                2 => thread.committed_clock_value += 1,
                3 => observed.completed_slices = u64::MAX,
                4 => {}
                _ => unreachable!(),
            }
            let before = observed.clone();
            let slices = thread.stats.timeslice_count;
            let end = thread.end_of_timeslice;
            let max = thread.max_timeslice_end;
            let clock = thread.thread_logical_time.as_nanos();
            assert!(
                observed
                    .close_slice(&mut thread, &cfg, bad == 4, None)
                    .is_err()
            );
            assert_eq!(observed, before);
            assert_eq!(thread.stats.timeslice_count, slices);
            assert_eq!(thread.end_of_timeslice, end);
            assert_eq!(thread.max_timeslice_end, max);
            assert_eq!(thread.thread_logical_time.as_nanos(), clock);
        }
    }

    #[test]
    fn pmu_no_rcb_and_software_callbacks_are_not_inferred_from_rip() {
        for (entry, cause) in [
            (Entry::Cpuid, Cause::SoftwareCheckin),
            (Entry::PmuTimer, Cause::PmuTimer),
            (Entry::NoRcbTimer, Cause::NoRcbTimer),
        ] {
            let (cfg, mut thread, mut observed) = fixture();
            observed.callback(&thread, entry, Phase::Prehook).unwrap();
            let closed = observed
                .close_slice(&mut thread, &cfg, false, None)
                .unwrap();
            assert_eq!(closed.boundary.identity.cause, cause);
            assert_eq!(closed.boundary.end_rip, None);
        }
    }

    #[test]
    fn posthook_and_counter_overflow_fail_without_advancing_observation() {
        let (_, thread, mut observed) = fixture();
        let before = observed.clone();
        assert!(
            observed
                .syscall(&thread, Sysno::read, Phase::Posthook)
                .is_err()
        );
        assert_eq!(observed, before);
        observed.checkin = u64::MAX;
        let before = observed.clone();
        assert!(
            observed
                .syscall(&thread, Sysno::read, Phase::Prehook)
                .is_err()
        );
        assert_eq!(observed, before);
    }

    #[test]
    fn boundary_software_expiry_requires_actual_entry_phase_rcbs_and_occurrence() {
        let (config, mut thread, mut observed) = fixture();
        // Any syscall will do; aarch64 has ppoll, not poll.
        #[cfg(target_arch = "x86_64")]
        let sysno = Sysno::poll;
        #[cfg(not(target_arch = "x86_64"))]
        let sysno = Sysno::ppoll;
        observed.syscall(&thread, sysno, Phase::Prehook).unwrap();
        let mut recorded_thread = ThreadState::new(thread.dettid, &config, ());
        let mut recorded = observed.clone();
        recorded
            .syscall(&recorded_thread, sysno, Phase::Posthook)
            .unwrap();
        let marker = recorded
            .close_slice(&mut recorded_thread, &config, false, None)
            .unwrap()
            .boundary;
        let schedule = Schedule::new(vec![Record::TimesliceEnd(marker)]).unwrap();
        let hint = grant::Budget::at(&schedule, 0).unwrap();
        thread.slice_boundary = Some(observed);
        grant::install(&mut thread, hint).unwrap();
        let clocks = (
            thread.committed_clock_value,
            thread.thread_logical_time.as_nanos(),
            thread.end_of_timeslice,
            thread.max_timeslice_end,
        );
        assert_eq!(replay_expiration(&thread).unwrap(), Some(false));
        let mut observed = thread.slice_boundary.take().unwrap();
        observed.syscall(&thread, sysno, Phase::Posthook).unwrap();
        thread.slice_boundary = Some(observed);
        assert_eq!(replay_expiration(&thread).unwrap(), Some(true));
        assert_eq!(
            clocks,
            (
                thread.committed_clock_value,
                thread.thread_logical_time.as_nanos(),
                thread.end_of_timeslice,
                thread.max_timeslice_end
            )
        );
        for field in 0..5 {
            let mut bad = thread.slice_boundary.clone().unwrap();
            match field {
                0 => bad.checkin += 1,
                1 => bad.current.as_mut().unwrap().0 = Entry::Cpuid,
                2 => bad.current.as_mut().unwrap().1 = Phase::Prehook,
                3 => bad.owner.birth_generation += 1,
                4 => bad.current.as_mut().unwrap().2 += 1,
                _ => unreachable!(),
            }
            assert!(bad.replay_expiration(&thread).is_err(), "field {field}");
        }
        thread.own_signal_turn_hold = Some(libc::SIGUSR1);
        assert!(replay_expiration(&thread).is_err());
    }

    #[test]
    fn boundary_explicit_yield_is_consumed_only_by_its_original_handler() {
        let (config, mut thread, mut observed) = fixture();
        observed
            .syscall(&thread, Sysno::sched_yield, Phase::Prehook)
            .unwrap();
        let mut copy = observed.clone();
        let mut record_thread = ThreadState::new(thread.dettid, &config, ());
        let marker = copy
            .close_slice(&mut record_thread, &config, true, None)
            .unwrap()
            .boundary;
        let schedule = Schedule::new(vec![Record::TimesliceEnd(marker)]).unwrap();
        thread.slice_boundary = Some(observed);
        grant::install(&mut thread, grant::Budget::at(&schedule, 0).unwrap()).unwrap();
        assert_eq!(replay_expiration(&thread).unwrap(), Some(false));
        let mut observed = thread.slice_boundary.take().unwrap();
        let before = thread.stats.timeslice_count;
        assert!(
            observed
                .close_slice(&mut thread, &config, false, None)
                .is_err()
        );
        assert_eq!(thread.stats.timeslice_count, before);
        observed
            .close_slice(&mut thread, &config, true, None)
            .unwrap();
        assert_eq!(thread.stats.timeslice_count, before + 1);
    }

    #[test]
    fn boundary_no_rcb_maximum_is_distinct_from_its_earlier_prehook() {
        let (config, mut thread, mut observed) = fixture();
        observed
            .callback(&thread, Entry::NoRcbTimer, Phase::Prehook)
            .unwrap();
        let mut copy = observed.clone();
        let mut record_thread = ThreadState::new(thread.dettid, &config, ());
        copy.publish(&record_thread, Entry::NoRcbTimer, Phase::TimerMaximum)
            .unwrap();
        let marker = copy
            .close_slice(&mut record_thread, &config, false, None)
            .unwrap()
            .boundary;
        let schedule = Schedule::new(vec![Record::TimesliceEnd(marker)]).unwrap();
        thread.slice_boundary = Some(observed);
        grant::install(&mut thread, grant::Budget::at(&schedule, 0).unwrap()).unwrap();
        assert_eq!(replay_expiration(&thread).unwrap(), Some(false));
        no_rcb_maximum(&mut thread).unwrap();
        assert_eq!(replay_expiration(&thread).unwrap(), Some(true));
        let mut observed = thread.slice_boundary.take().unwrap();
        assert_eq!(
            observed
                .close_slice(&mut thread, &config, false, None)
                .unwrap()
                .boundary
                .identity
                .phase,
            Phase::TimerMaximum
        );
    }
}
