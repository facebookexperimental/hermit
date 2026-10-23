/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Identity allocated only by the scheduler's existing authenticated lifecycle
//! transitions. Counters are local to deterministic TID slots: unrelated host
//! registration order cannot select another thread's identity.

use std::collections::BTreeMap;

use anyhow::bail;
use detcore_model::futex::MmId;
use detcore_model::pid::DetTid;
use detcore_model::schedule::boundary::Boundary;
use detcore_model::schedule::boundary::Identity;
use detcore_model::schedule::boundary::Owner;

#[derive(Debug, Default)]
pub(crate) struct Registry {
    last: BTreeMap<DetTid, u64>,
    live: BTreeMap<DetTid, Owner>,
    boundaries: BTreeMap<Owner, Identity>,
}

impl Registry {
    pub(crate) fn validate_boundary(
        &self,
        tid: DetTid,
        mm: MmId,
        boundary: &Boundary,
    ) -> anyhow::Result<()> {
        boundary.validate_shape(0)?;
        let identity = boundary.identity;
        if identity.owner != self.owner(tid, mm)? {
            bail!(
                "slice-boundary protocol refusal: boundary does not name the authenticated owner"
            );
        }
        match self.boundaries.get(&identity.owner) {
            None if identity.slice != 1 => {
                bail!("slice-boundary protocol refusal: missing initial slice")
            }
            Some(last)
                if last.slice.checked_add(1) != Some(identity.slice)
                    || identity.checkin < last.checkin
                    || identity.committed_rcbs < last.committed_rcbs
                    || (identity.checkin == last.checkin
                        && (identity.entry != last.entry
                            || identity.phase != last.phase
                            || identity.committed_rcbs != last.committed_rcbs)) =>
            {
                bail!("slice-boundary protocol refusal: repeated or reordered boundary")
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn commit_boundary(&mut self, boundary: &Boundary) {
        self.boundaries
            .insert(boundary.identity.owner, boundary.identity);
    }

    fn next(&self, tid: DetTid) -> anyhow::Result<u64> {
        if tid.as_raw() <= 0 {
            bail!("slice-boundary protocol refusal: invalid lifecycle TID");
        }
        match self.last.get(&tid) {
            None => Ok(0),
            Some(last) => last.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!("slice-boundary protocol refusal: TID incarnation overflow")
            }),
        }
    }

    pub(crate) fn fresh(&mut self, tid: DetTid, mm: MmId) -> anyhow::Result<Owner> {
        if self.live.contains_key(&tid) {
            bail!("slice-boundary protocol refusal: duplicate live thread registration");
        }
        let generation = self.next(tid)?;
        let owner = Owner {
            tid,
            thread_generation: generation,
            birth_tid: tid,
            birth_generation: generation,
            address_space_creator: mm.creator(),
            address_space_generation: mm.generation(),
        };
        self.last.insert(tid, generation);
        self.live.insert(tid, owner);
        Ok(owner)
    }

    pub(crate) fn owner(&self, tid: DetTid, mm: MmId) -> anyhow::Result<Owner> {
        match self.live.get(&tid).copied() {
            Some(owner)
                if owner.address_space_creator == mm.creator()
                    && owner.address_space_generation == mm.generation() =>
            {
                Ok(owner)
            }
            _ => bail!("slice-boundary protocol refusal: unregistered or stale lifecycle owner"),
        }
    }

    /// Called with the successful exec's authenticated old/new MM pair. The
    /// logical caller survives; the leader slot's former occupant does not.
    /// All checks precede mutation, including incarnation overflow.
    pub(crate) fn exec(
        &mut self,
        caller: DetTid,
        leader: DetTid,
        old_mm: MmId,
        new_mm: MmId,
    ) -> anyhow::Result<Owner> {
        let survivor = self.owner(caller, old_mm)?;
        if new_mm.creator() != leader
            || old_mm.generation().checked_add(1) != Some(new_mm.generation())
        {
            bail!("slice-boundary protocol refusal: invalid exec address-space transition");
        }
        let generation = if caller == leader {
            survivor.thread_generation
        } else {
            if let Some(displaced) = self.live.get(&leader)
                && (displaced.address_space_creator != old_mm.creator()
                    || displaced.address_space_generation != old_mm.generation())
            {
                bail!("slice-boundary protocol refusal: exec displaced an unrelated owner");
            }
            self.next(leader)?
        };
        let owner = Owner {
            tid: leader,
            thread_generation: generation,
            address_space_creator: new_mm.creator(),
            address_space_generation: new_mm.generation(),
            ..survivor
        };
        self.live.remove(&caller);
        self.last.insert(leader, generation);
        self.live.insert(leader, owner);
        Ok(owner)
    }

    /// An old incarnation's delayed retirement must not remove its exec
    /// replacement. The existing lifecycle caller owns retirement authority.
    pub(crate) fn retire(&mut self, tid: DetTid, mm: MmId) {
        if self.owner(tid, mm).is_ok() {
            self.live.remove(&tid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(raw: i32) -> DetTid {
        DetTid::from_raw(raw)
    }

    #[test]
    fn boundary_lifecycle_reuse_and_unrelated_registration_order() {
        let mut a = Registry::default();
        let mut b = Registry::default();
        let first = a.fresh(tid(3), MmId::initial(tid(3))).unwrap();
        a.fresh(tid(4), MmId::initial(tid(4))).unwrap();
        b.fresh(tid(4), MmId::initial(tid(4))).unwrap();
        assert_eq!(first, b.fresh(tid(3), MmId::initial(tid(3))).unwrap());
        assert!(a.fresh(tid(3), MmId::initial(tid(3))).is_err());
        assert_eq!(a.owner(tid(3), MmId::initial(tid(3))).unwrap(), first);
        a.retire(tid(3), MmId::initial(tid(3)));
        let second = a.fresh(tid(3), MmId::initial(tid(3))).unwrap();
        assert_eq!(second.thread_generation, 1);
        assert_eq!(second.birth_generation, 1);
        assert_ne!(first, second);
    }

    #[test]
    fn boundary_lifecycle_nonleader_exec_retains_birth_not_displaced_identity() {
        let mut registry = Registry::default();
        let mm = MmId::initial(tid(3));
        let leader = registry.fresh(tid(3), mm).unwrap();
        let survivor = registry.fresh(tid(4), mm).unwrap();
        let replacement_mm = mm.for_exec(tid(3));
        let replacement = registry.exec(tid(4), tid(3), mm, replacement_mm).unwrap();
        assert_eq!(replacement.tid, leader.tid);
        assert_eq!(replacement.thread_generation, leader.thread_generation + 1);
        assert_eq!(replacement.birth_tid, survivor.birth_tid);
        assert_eq!(replacement.birth_generation, survivor.birth_generation);
        assert!(registry.owner(tid(4), mm).is_err());
        assert!(registry.owner(tid(3), mm).is_err());
        registry.retire(tid(3), mm);
        registry.retire(tid(4), mm);
        assert_eq!(registry.owner(tid(3), replacement_mm).unwrap(), replacement);
        let reused = registry.fresh(tid(4), MmId::initial(tid(4))).unwrap();
        assert_ne!(reused.birth_generation, survivor.birth_generation);
        let another_mm = replacement_mm.for_exec(tid(3));
        let another = registry
            .exec(tid(3), tid(3), replacement_mm, another_mm)
            .unwrap();
        assert_eq!(another.birth_tid, survivor.birth_tid);
        assert_eq!(another.thread_generation, replacement.thread_generation);
    }

    #[test]
    fn boundary_lifecycle_refuses_stale_unrelated_and_overflow_without_mutation() {
        let mut registry = Registry::default();
        let mm = MmId::initial(tid(3));
        let survivor = registry.fresh(tid(4), mm).unwrap();
        let unrelated = registry.fresh(tid(3), MmId::initial(tid(5))).unwrap();
        let before = registry.live.clone();
        assert!(
            registry
                .exec(tid(4), tid(3), mm, mm.for_exec(tid(3)))
                .is_err()
        );
        assert!(registry.exec(tid(4), tid(4), mm, mm).is_err());
        assert!(
            registry
                .exec(tid(4), tid(4), MmId::initial(tid(6)), mm.for_exec(tid(4)))
                .is_err()
        );
        assert_eq!(registry.live, before);
        registry.retire(tid(3), MmId::initial(tid(5)));
        registry.last.insert(tid(3), u64::MAX);
        let before = registry.live.clone();
        assert!(
            registry
                .exec(tid(4), tid(3), mm, mm.for_exec(tid(3)))
                .is_err()
        );
        assert!(registry.fresh(tid(3), mm).is_err());
        assert_eq!(registry.live, before);
        assert_eq!(registry.owner(tid(4), mm).unwrap(), survivor);
        assert_eq!(unrelated.birth_tid, tid(3));
    }

    #[test]
    fn boundary_lifecycle_reply_preserves_clock_and_refuses_expected_owner_substitution() {
        use crate::ThreadState;
        use crate::config::Config;
        use crate::slice_boundary::adopt_owner;
        let config = Config::default();
        let mut thread = ThreadState::new(tid(4), &config, ());
        let mm = thread.mm_id;
        let mut registry = Registry::default();
        let owner = registry.fresh(thread.dettid, mm).unwrap();
        assert!(adopt_owner(&mut thread, owner, false).is_err());
        assert!(thread.slice_boundary.is_none());
        adopt_owner(&mut thread, owner, true).unwrap();
        let state = thread.slice_boundary.clone();
        adopt_owner(&mut thread, owner, false).unwrap();
        assert_eq!(thread.slice_boundary, state);
        let before = (
            thread.committed_clock_value,
            thread.thread_logical_time.as_nanos(),
            thread.stats.timeslice_count,
            thread.end_of_timeslice,
            thread.max_timeslice_end,
        );
        let new_mm = mm.for_exec(tid(3));
        let replacement = registry.exec(tid(4), tid(3), mm, new_mm).unwrap();
        assert!(adopt_owner(&mut thread, replacement, false).is_err());
        assert_eq!(thread.slice_boundary, state);
        thread.dettid = tid(3);
        thread.mm_id = new_mm;
        let mut invented = replacement;
        invented.birth_tid = tid(5);
        assert!(adopt_owner(&mut thread, invented, false).is_err());
        assert_eq!(thread.slice_boundary, state);
        thread.replay_rcb_end = Some(999);
        adopt_owner(&mut thread, replacement, false).unwrap();
        assert_eq!(thread.slice_boundary.as_ref().unwrap().owner, replacement);
        assert_eq!(thread.replay_rcb_end, None);
        assert_eq!(
            (
                thread.committed_clock_value,
                thread.thread_logical_time.as_nanos(),
                thread.stats.timeslice_count,
                thread.end_of_timeslice,
                thread.max_timeslice_end
            ),
            before
        );
    }
}
