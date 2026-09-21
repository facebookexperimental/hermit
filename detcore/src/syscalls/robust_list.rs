/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic emulation of Linux's robust-futex owner-death protocol.
//!
//! Linux keeps a per-task singly linked list of the robust futexes a thread
//! currently owns (`set_robust_list(2)`). When the task dies, `mm_release()`
//! calls `exit_robust_list()`, which walks that list and, for every futex word
//! that still names the dying task, atomically replaces the word with
//! `(uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED` and — for non-PI futexes with
//! waiters — wakes exactly one waiter. glibc's `pthread_mutex_lock` then
//! observes `FUTEX_OWNER_DIED` and returns `EOWNERDEAD`.
//!
//! Detcore's precise futex model parks waiters in the scheduler's own
//! `futex_waiters` pool rather than in a kernel futex queue, so the kernel's
//! internal owner-death wake cannot reach them. This module re-implements the
//! kernel algorithm inside Detcore so the wake is issued against the modeled
//! waiter pool.
//!
//! On ptrace, Detcore performs the modeled wake while leaving the owner-word
//! transition to the real Linux task-exit path:
//!
//! * Linux performs the required compare/exchange atomically, so a process
//!   outside Hermit's scheduler cannot have its newly acquired process-shared
//!   mutex overwritten by a separate Detcore read and write;
//! * waking a modeled waiter does not immediately run it. Ptrace keeps the
//!   dying thread's handler pending through its tail-injected exit, so Linux
//!   changes the word before the exit callback releases another scheduler turn.
//!
//! DBT and SaBRe invoke their task-exit callback before executing the native
//! exit, and KVM has no Linux task-exit path. Those backends do not run this
//! owner-death path until they can provide an atomic update or a post-cleanup
//! callback. The fake below models the unsafe separate read/write window so a
//! future implementation cannot hide it behind an atomic test double.
//!
//! # Lifecycle scope
//!
//! Ptrace mirrors every thread's registration and can read all sibling lists
//! before `exit_group` or a Guest-observed fatal signal destroys the address
//! space. It holds those modeled wakes until the physical-exit callback, after
//! Linux has performed the atomic owner-word transition. Fatal signals that
//! terminate an injected blocking syscall do not currently provide that
//! Guest-bearing callback, and successful exec's `de_thread` path is blocked by
//! a separate ptrace exec-lifecycle failure. DBT, KVM, and SaBRe also need
//! backend-specific lifecycle or atomic-memory support.
//!
//! # Structure
//!
//! [`exit_robust_list`] is the whole algorithm, written against the
//! [`RobustDeathEffects`] trait rather than against a `Guest`, so the walk —
//! ordering, the pending slot, the fault-abort rules and the
//! `ROBUST_LIST_LIMIT` bound — is unit-testable over fake guest memory with no
//! backend, no scheduler and no tracee. `threads.rs` supplies the one real
//! implementation.

use tracing::trace;

/// `FUTEX_WAITERS` from `include/uapi/linux/futex.h`.
pub(crate) const FUTEX_WAITERS: u32 = 0x8000_0000;
/// `FUTEX_OWNER_DIED` from `include/uapi/linux/futex.h`.
pub(crate) const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
/// `FUTEX_TID_MASK` from `include/uapi/linux/futex.h`.
pub(crate) const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

/// `ROBUST_LIST_LIMIT` from `kernel/futex/core.c`: the kernel refuses to follow
/// more than this many entries, which bounds a corrupt or circular list.
pub(crate) const ROBUST_LIST_LIMIT: usize = 2048;

/// Byte offset of `robust_list_head.list.next`.
pub(crate) const HEAD_LIST_OFFSET: usize = 0;
/// Byte offset of `robust_list_head.futex_offset`.
pub(crate) const HEAD_FUTEX_OFFSET_OFFSET: usize = 8;
/// Byte offset of `robust_list_head.list_op_pending`.
pub(crate) const HEAD_LIST_OP_PENDING_OFFSET: usize = 16;
/// `sizeof(struct robust_list_head)` on 64-bit Linux. `set_robust_list(2)`
/// rejects any other length with `EINVAL`.
pub(crate) const ROBUST_LIST_HEAD_LEN: usize = 24;

/// One decoded `struct robust_list *` slot.
///
/// The kernel's `fetch_robust_entry()` stores the PI flag in bit 0 of the
/// pointer, so the entry address is the pointer with that bit cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RobustEntry {
    /// Guest address of the `struct robust_list` node, PI bit removed.
    pub address: usize,
    /// Whether this entry describes a priority-inheritance futex.
    pub is_pi: bool,
}

impl RobustEntry {
    pub(crate) fn decode(raw: u64) -> Self {
        Self {
            address: (raw & !1u64) as usize,
            is_pi: raw & 1 != 0,
        }
    }

    /// Whether this slot is the NULL terminator (`fetch_robust_entry` yields a
    /// null entry only for an unset `list_op_pending`).
    pub(crate) fn is_null(&self) -> bool {
        self.address == 0
    }
}

/// Resolve the futex word address for a list node, mirroring the kernel's
/// `(void __user *)entry + futex_offset`.
///
/// `futex_offset` is a signed value (glibc uses a negative one, because the
/// list node lives after the lock word inside `pthread_mutex_t`). Returns
/// `None` when the sum is not a plausible user address, which the kernel would
/// have discovered as a `get_user()` fault.
pub(crate) fn futex_word_address(entry: usize, futex_offset: i64) -> Option<usize> {
    let offset = isize::try_from(futex_offset).ok()?;
    let sum = entry.checked_add_signed(offset)?;
    (sum != 0).then_some(sum)
}

/// The state change `handle_futex_death()` applies to one futex word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FutexDeathTransition {
    /// Value to store back into the futex word.
    pub new_value: u32,
    /// Whether exactly one waiter must be woken afterwards.
    pub wake_one: bool,
}

/// Pure model of the kernel's `handle_futex_death()` decision.
///
/// Returns `None` when the word does not name `owner_tid`, in which case Linux
/// leaves the word untouched and issues no wake.
pub(crate) fn futex_death_transition(
    uval: u32,
    owner_tid: u32,
    is_pi: bool,
) -> Option<FutexDeathTransition> {
    if uval & FUTEX_TID_MASK != owner_tid & FUTEX_TID_MASK {
        return None;
    }
    Some(FutexDeathTransition {
        new_value: (uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED,
        // `handle_futex_death()`: `if (!pi && (uval & FUTEX_WAITERS))`.
        wake_one: !is_pi && uval & FUTEX_WAITERS != 0,
    })
}

/// What `handle_futex_death()` told the walk to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeathStep {
    /// The kernel's `return 0`: this entry is finished, keep walking.
    Continue,
    /// The kernel's `return -1`. `exit_robust_list()` propagates it with a bare
    /// `return`, so nothing further is touched — not the remaining entries and
    /// not the trailing `list_op_pending` slot.
    Abort,
}

/// What one attempted futex-word replacement reported, mirroring the kernel's
/// `futex_cmpxchg_value_locked()` and the `nval != uval` test that follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FutexCasOutcome {
    /// The word still held the expected value and now holds the new one.
    Stored,
    /// The backend will perform the atomic replacement before the modeled wake
    /// can let another thread run.
    Deferred,
    /// The word held this other value instead, so nothing was written. The
    /// kernel's `goto retry`.
    Changed(u32),
    /// The access faulted and could not be completed. The kernel's `return -1`
    /// after `fault_in_user_writeable()` also fails.
    Faulted,
}

/// Guest-memory and wake effects the walk needs.
///
/// Splitting these out is what makes [`exit_robust_list`] testable: the walk
/// never mentions `Guest`, `Detcore` or the scheduler, so a test can drive the
/// real algorithm over a map of bytes and assert the exact sequence of writes
/// and wakes.
pub(crate) trait RobustDeathEffects {
    /// Read a `u64` from guest memory. `None` models `get_user()` faulting.
    fn read_u64(&mut self, address: usize) -> Option<u64>;

    /// Read the 32-bit futex word. `None` models `get_user()` faulting.
    fn read_u32(&mut self, address: usize) -> Option<u32>;

    /// Arrange to replace the futex word, but only while it still holds `expected`.
    ///
    /// This is `futex_cmpxchg_value_locked(&nval, uaddr, uval, mval)` plus the
    /// `if (nval != uval) goto retry;` immediately after it: the kernel never
    /// stores `desired` over a word whose value moved since it was read,
    /// because that word may have been re-acquired by a live thread. A backend
    /// whose Linux task-exit cleanup completes before another modeled thread
    /// can run may return [`FutexCasOutcome::Deferred`] and leave the atomic
    /// replacement to it.
    fn compare_and_swap(&mut self, address: usize, expected: u32, desired: u32) -> FutexCasOutcome;

    /// Wake exactly one waiter on the futex word, as the kernel's
    /// `futex_wake(uaddr, 1, ...)` does.
    ///
    /// `observed` is the word's value at this point, carried so the caller can
    /// pass it to the scheduler RPC and log it.
    async fn wake_one(&mut self, address: usize, observed: u32);
}

/// Why a walk stopped, for logging and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct WalkOutcome {
    /// Entries handed to `handle_futex_death()`, including the pending slot.
    pub entries_visited: usize,
    /// A `robust_list_head` field could not be read; nothing was touched.
    pub head_unreadable: bool,
    /// `handle_futex_death()` returned `-1`, abandoning the walk.
    pub aborted: bool,
    /// A `next` pointer faulted, ending the walk after the current entry.
    pub next_faulted: bool,
    /// `ROBUST_LIST_LIMIT` entries were followed without reaching the head.
    pub truncated: bool,
    /// How many times a futex word changed between being read and being
    /// written, forcing the kernel's `goto retry`.
    pub cas_retries: usize,
}

/// Replay Linux's `exit_robust_list()` (`kernel/futex/core.c`) over `effects`.
///
/// `owner_tid` is the dying thread's guest-visible TID: the value that must
/// appear in a futex word's `FUTEX_TID_MASK` field for that word to be marked.
pub(crate) async fn exit_robust_list<E: RobustDeathEffects>(
    effects: &mut E,
    head: usize,
    owner_tid: u32,
) -> WalkOutcome {
    let mut outcome = WalkOutcome::default();

    // `exit_robust_list()` fetches list.next, futex_offset and list_op_pending
    // up front and returns immediately if any of the three faults.
    let (Some(first_raw), Some(futex_offset), Some(pending_raw)) = (
        head.checked_add(HEAD_LIST_OFFSET)
            .and_then(|at| effects.read_u64(at)),
        head.checked_add(HEAD_FUTEX_OFFSET_OFFSET)
            .and_then(|at| effects.read_u64(at)),
        head.checked_add(HEAD_LIST_OP_PENDING_OFFSET)
            .and_then(|at| effects.read_u64(at)),
    ) else {
        outcome.head_unreadable = true;
        return outcome;
    };
    let futex_offset = futex_offset as i64;
    let mut entry = RobustEntry::decode(first_raw);
    let pending = {
        let decoded = RobustEntry::decode(pending_raw);
        (!decoded.is_null()).then_some(decoded)
    };

    let mut limit = ROBUST_LIST_LIMIT;
    while entry.address != head {
        // `fetch_robust_entry(&next_entry, &entry->next, &next_pi)` runs BEFORE
        // this entry is handled, because handling it can rewrite guest memory.
        // `struct robust_list` is a single `next` pointer at offset 0.
        let next = effects.read_u64(entry.address).map(RobustEntry::decode);

        // The kernel skips `pending` here and handles it once at the end.
        if pending.map(|slot| slot.address) != Some(entry.address) {
            outcome.entries_visited += 1;
            if handle_futex_death(
                effects,
                entry,
                futex_offset,
                owner_tid,
                false,
                &mut outcome.cas_retries,
            )
            .await
                == DeathStep::Abort
            {
                outcome.aborted = true;
                return outcome;
            }
        }

        // The kernel tests `fetch_robust_entry`'s `rc` only after handling the
        // current entry, so a faulting `next` still lets this entry run.
        let Some(next) = next else {
            outcome.next_faulted = true;
            return outcome;
        };
        entry = next;
        limit -= 1;
        if limit == 0 {
            outcome.truncated = true;
            break;
        }
    }

    // `list_op_pending` covers the window in which the guest has claimed or
    // released a mutex but not yet linked or unlinked it, so the kernel handles
    // it last and with `pending_op` set.
    if let Some(pending) = pending {
        outcome.entries_visited += 1;
        if handle_futex_death(
            effects,
            pending,
            futex_offset,
            owner_tid,
            true,
            &mut outcome.cas_retries,
        )
        .await
            == DeathStep::Abort
        {
            outcome.aborted = true;
        }
    }

    outcome
}

/// Replay the kernel's `handle_futex_death()` for one list entry.
///
/// `cas_retries` accumulates every `goto retry` taken because the futex word
/// changed between being read and being written.
async fn handle_futex_death<E: RobustDeathEffects>(
    effects: &mut E,
    entry: RobustEntry,
    futex_offset: i64,
    owner_tid: u32,
    pending_op: bool,
    cas_retries: &mut usize,
) -> DeathStep {
    let Some(word) = futex_word_address(entry.address, futex_offset) else {
        // The kernel computes the same wild address and faults in `get_user()`,
        // which is a `-1` return, not a skip.
        return DeathStep::Abort;
    };
    // "Futex address must be 32bit aligned" is `handle_futex_death()`'s first
    // statement, before any read: Linux refuses a misaligned word outright
    // rather than performing an unaligned read-modify-write on it.
    if word % std::mem::size_of::<u32>() != 0 {
        trace!("robust-list entry has a misaligned futex word {:#x}", word);
        return DeathStep::Abort;
    }

    // Linux's `retry:` label. Every decision below is made from a freshly read
    // word and is only committed by a compare-and-swap against that same word,
    // so a futex re-acquired by a live thread in the meantime is never stamped
    // with the dying thread's `FUTEX_OWNER_DIED`. The kernel's loop is
    // unbounded — its only exits are a successful store, a word that no longer
    // names the dying owner, and a fault — and this one is deliberately the
    // same shape rather than a bound this code would have had to invent.
    loop {
        let Some(uval) = effects.read_u32(word) else {
            // `if (get_user(uval, uaddr)) return -1;`
            return DeathStep::Abort;
        };

        // The pending-op special case. The owner died between releasing the
        // futex word and issuing its wake, or after being woken but before
        // taking the lock, so a waiter may be parked on an already-free futex.
        // Linux wakes one waiter WITHOUT marking the word: setting
        // `FUTEX_OWNER_DIED` on a zero word would create exactly the
        // inconsistent state that user-space owner-died handling cannot
        // recover from. The kernel re-tests this after every retry too,
        // because the word may have reached zero in the meantime.
        if pending_op && !entry.is_pi && uval & FUTEX_TID_MASK == 0 {
            effects.wake_one(word, uval).await;
            return DeathStep::Continue;
        }

        let Some(transition) = futex_death_transition(uval, owner_tid, entry.is_pi) else {
            // `if ((uval & FUTEX_TID_MASK) != task_pid_vnr(curr)) return 0;`
            // On the first pass this is an entry the dying thread never owned.
            // On a retry it is the race this loop exists for: the futex was
            // acquired by another thread after the read, and marking it now
            // would tell that live owner's waiters the lock is abandoned.
            return DeathStep::Continue;
        };

        match effects.compare_and_swap(word, uval, transition.new_value) {
            FutexCasOutcome::Stored | FutexCasOutcome::Deferred => {
                if transition.wake_one {
                    effects.wake_one(word, transition.new_value).await;
                }
                return DeathStep::Continue;
            }
            FutexCasOutcome::Changed(observed) => {
                // `if (nval != uval) goto retry;`
                *cas_retries += 1;
                trace!(
                    "robust-list futex word {:#x} moved {:#x} -> {:#x} under the walk; retrying",
                    word, uval, observed,
                );
            }
            FutexCasOutcome::Faulted => return DeathStep::Abort,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::collections::VecDeque;

    use super::*;

    /// glibc's `futex_offset` is
    /// `offsetof(pthread_mutex_t, __data.__lock) - offsetof(pthread_mutex_t, __data.__list.__next)`,
    /// i.e. -32 on x86-64.
    const GLIBC_FUTEX_OFFSET: i64 = -32;

    const OWNER: u32 = 5;

    /// Byte-addressed fake guest memory plus a recorder for every effect the
    /// walk performs, so a test can assert the exact sequence.
    #[derive(Debug, Default)]
    struct FakeGuest {
        bytes: BTreeMap<usize, u8>,
        /// Addresses whose read faults.
        unreadable: BTreeSet<usize>,
        /// Addresses whose write faults.
        unwritable: BTreeSet<usize>,
        /// Values a concurrent writer stores into a word immediately after the
        /// walk reads it, one per read, consumed in order. This is the only way
        /// to reach the compare-and-swap retry path from a test, because the
        /// walk itself is single-threaded.
        races: BTreeMap<usize, VecDeque<u32>>,
        /// Values stored after `compare_and_swap` has re-read and accepted the
        /// expected word, but before its separate write. This reproduces the
        /// production read/write window rather than granting the fake an
        /// atomic operation production does not have.
        races_before_write: BTreeMap<usize, VecDeque<u32>>,
        /// Leave the owner-word transition to the backend's task-exit cleanup.
        defer_owner_death_to_backend: bool,
        /// `(word, before, after)` for each applied transition, in order.
        marks: Vec<(usize, u32, u32)>,
        /// `(word, observed)` for each wake, in order.
        wakes: Vec<(usize, u32)>,
    }

    impl FakeGuest {
        /// Arrange for `value` to appear in `address` after the next read of
        /// it, as another thread's store would.
        fn race_after_read(&mut self, address: usize, value: u32) {
            self.races.entry(address).or_default().push_back(value);
        }

        fn race_before_write(&mut self, address: usize, value: u32) {
            self.races_before_write
                .entry(address)
                .or_default()
                .push_back(value);
        }

        fn put_u64(&mut self, address: usize, value: u64) {
            for (i, byte) in value.to_le_bytes().into_iter().enumerate() {
                self.bytes.insert(address + i, byte);
            }
        }

        fn put_u32(&mut self, address: usize, value: u32) {
            for (i, byte) in value.to_le_bytes().into_iter().enumerate() {
                self.bytes.insert(address + i, byte);
            }
        }

        fn get_u32(&self, address: usize) -> Option<u32> {
            let mut buf = [0u8; 4];
            for (i, slot) in buf.iter_mut().enumerate() {
                *slot = *self.bytes.get(&(address + i))?;
            }
            Some(u32::from_le_bytes(buf))
        }

        /// Lay down a `robust_list_head` with glibc's field layout.
        fn head(&mut self, head: usize, first: u64, pending: u64) {
            self.put_u64(head + HEAD_LIST_OFFSET, first);
            self.put_u64(head + HEAD_FUTEX_OFFSET_OFFSET, GLIBC_FUTEX_OFFSET as u64);
            self.put_u64(head + HEAD_LIST_OP_PENDING_OFFSET, pending);
        }

        /// Lay down one list node at `node` whose `next` is `next`, with a
        /// glibc-shaped futex word 32 bytes below it holding `word`.
        fn node(&mut self, node: usize, next: u64, word: u32) {
            self.put_u64(node, next);
            self.put_u32(node - 32, word);
        }

        fn marked_words(&self) -> Vec<usize> {
            self.marks.iter().map(|mark| mark.0).collect()
        }

        fn woken_words(&self) -> Vec<usize> {
            self.wakes.iter().map(|wake| wake.0).collect()
        }
    }

    impl RobustDeathEffects for FakeGuest {
        fn read_u64(&mut self, address: usize) -> Option<u64> {
            if self.unreadable.contains(&address) {
                return None;
            }
            let mut buf = [0u8; 8];
            for (i, slot) in buf.iter_mut().enumerate() {
                *slot = *self.bytes.get(&(address + i))?;
            }
            Some(u64::from_le_bytes(buf))
        }

        fn read_u32(&mut self, address: usize) -> Option<u32> {
            if self.unreadable.contains(&address) {
                return None;
            }
            let value = self.get_u32(address)?;
            // A racing store lands after the read has already taken its value.
            if let Some(raced) = self.races.get_mut(&address).and_then(VecDeque::pop_front) {
                self.put_u32(address, raced);
            }
            Some(value)
        }

        fn compare_and_swap(
            &mut self,
            address: usize,
            expected: u32,
            desired: u32,
        ) -> FutexCasOutcome {
            if self.unwritable.contains(&address) {
                return FutexCasOutcome::Faulted;
            }
            let Some(observed) = self.get_u32(address) else {
                return FutexCasOutcome::Faulted;
            };
            if observed != expected {
                return FutexCasOutcome::Changed(observed);
            }
            if let Some(raced) = self
                .races_before_write
                .get_mut(&address)
                .and_then(VecDeque::pop_front)
            {
                self.put_u32(address, raced);
            }
            if self.defer_owner_death_to_backend {
                return FutexCasOutcome::Deferred;
            }
            self.put_u32(address, desired);
            self.marks.push((address, expected, desired));
            FutexCasOutcome::Stored
        }

        async fn wake_one(&mut self, address: usize, observed: u32) {
            self.wakes.push((address, observed));
        }
    }

    fn walk(guest: &mut FakeGuest, head: usize, owner: u32) -> WalkOutcome {
        futures::executor::block_on(exit_robust_list(guest, head, owner))
    }

    // ---- pure helpers -----------------------------------------------------

    #[test]
    fn pi_bit_is_carried_in_the_pointer_low_bit() {
        assert_eq!(
            RobustEntry::decode(0x0040_4120),
            RobustEntry {
                address: 0x0040_4120,
                is_pi: false
            }
        );
        assert_eq!(
            RobustEntry::decode(0x0040_4121),
            RobustEntry {
                address: 0x0040_4120,
                is_pi: true
            }
        );
        assert!(RobustEntry::decode(0).is_null());
        assert!(!RobustEntry::decode(0x0040_4120).is_null());
    }

    #[test]
    fn glibc_negative_futex_offset_resolves_to_the_lock_word() {
        // Observed layout from the `robust_futex_test` reproducer: the mutex is
        // at 0x404100 and its list node at 0x404120.
        assert_eq!(
            futex_word_address(0x0040_4120, GLIBC_FUTEX_OFFSET),
            Some(0x0040_4100)
        );
    }

    #[test]
    fn implausible_word_addresses_are_refused_rather_than_wrapping() {
        assert_eq!(futex_word_address(8, GLIBC_FUTEX_OFFSET), None);
        assert_eq!(futex_word_address(0, 0), None);
        assert_eq!(futex_word_address(usize::MAX, i64::MAX), None);
    }

    #[test]
    fn a_word_owned_by_another_thread_is_left_alone() {
        // Owner 5 dies; the word names thread 7.
        assert_eq!(futex_death_transition(FUTEX_WAITERS | 7, 5, false), None);
        assert_eq!(futex_death_transition(0, 5, false), None);
    }

    #[test]
    fn owner_death_marks_the_word_and_wakes_one_waiter() {
        // 0x80000005: FUTEX_WAITERS set, owner TID 5. This is the exact value
        // the reproducer's waiter passes to FUTEX_WAIT.
        let transition = futex_death_transition(0x8000_0005, 5, false).unwrap();
        assert_eq!(transition.new_value, FUTEX_WAITERS | FUTEX_OWNER_DIED);
        assert!(transition.wake_one);
    }

    #[test]
    fn owner_death_without_waiters_marks_the_word_but_wakes_nobody() {
        let transition = futex_death_transition(5, 5, false).unwrap();
        assert_eq!(transition.new_value, FUTEX_OWNER_DIED);
        assert!(!transition.wake_one);
    }

    #[test]
    fn pi_entries_are_marked_but_never_plain_woken() {
        // Linux hands PI wakeups to the rt_mutex owner-boosting path instead of
        // `futex_wake()`; mirror that by marking the word only.
        let transition = futex_death_transition(0x8000_0005, 5, true).unwrap();
        assert_eq!(transition.new_value, FUTEX_WAITERS | FUTEX_OWNER_DIED);
        assert!(!transition.wake_one);
    }

    #[test]
    fn an_already_dead_word_is_not_re_marked() {
        // After Detcore applies the transition the TID field is zero, so a
        // second pass (for instance the host kernel's own `exit_robust_list`)
        // finds no match. This is what makes the emulation idempotent.
        let once = futex_death_transition(0x8000_0005, 5, false).unwrap();
        assert_eq!(futex_death_transition(once.new_value, 5, false), None);
    }

    #[test]
    fn high_tid_words_compare_under_the_tid_mask_only() {
        // FUTEX_WAITERS|FUTEX_OWNER_DIED|tid must not be mistaken for a
        // different owner because of the flag bits.
        let uval = FUTEX_WAITERS | FUTEX_OWNER_DIED | 0x0000_1234;
        assert!(futex_death_transition(uval, 0x0000_1234, false).is_some());
        assert!(futex_death_transition(uval, 0x0000_1235, false).is_none());
    }

    // ---- the walk itself --------------------------------------------------

    #[test]
    fn an_empty_list_touches_nothing() {
        let mut guest = FakeGuest::default();
        // A registered but empty list points `list.next` back at the head.
        guest.head(0x1000, 0x1000, 0);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(outcome, WalkOutcome::default());
        assert!(guest.marks.is_empty());
        assert!(guest.wakes.is_empty());
    }

    #[test]
    fn one_held_mutex_with_a_waiter_is_marked_then_woken() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x1000, FUTEX_WAITERS | OWNER);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(outcome.entries_visited, 1);
        assert!(!outcome.aborted && !outcome.truncated && !outcome.next_faulted);
        assert_eq!(
            guest.marks,
            vec![(
                0x2000,
                FUTEX_WAITERS | OWNER,
                FUTEX_WAITERS | FUTEX_OWNER_DIED
            )]
        );
        assert_eq!(
            guest.wakes,
            vec![(0x2000, FUTEX_WAITERS | FUTEX_OWNER_DIED)]
        );
        assert_eq!(
            guest.get_u32(0x2000),
            Some(FUTEX_WAITERS | FUTEX_OWNER_DIED)
        );
    }

    #[test]
    fn several_entries_are_marked_in_guest_pointer_order() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x3020, FUTEX_WAITERS | OWNER);
        guest.node(0x3020, 0x4020, OWNER);
        guest.node(0x4020, 0x1000, FUTEX_WAITERS | OWNER);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(outcome.entries_visited, 3);
        // Order follows the guest's own pointers, which is deterministic; the
        // uncontended entry is marked but issues no wake.
        assert_eq!(guest.marked_words(), vec![0x2000, 0x3000, 0x4000]);
        assert_eq!(guest.woken_words(), vec![0x2000, 0x4000]);
    }

    #[test]
    fn entries_owned_by_another_thread_are_skipped_without_stopping_the_walk() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x3020, FUTEX_WAITERS | 7);
        guest.node(0x3020, 0x1000, FUTEX_WAITERS | OWNER);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(outcome.entries_visited, 2);
        assert_eq!(guest.marked_words(), vec![0x3000]);
        assert_eq!(guest.get_u32(0x2000), Some(FUTEX_WAITERS | 7));
    }

    #[test]
    fn a_pi_entry_is_marked_but_not_woken_during_a_walk() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2021, 0); // low bit set => PI entry
        guest.node(0x2020, 0x1000, FUTEX_WAITERS | OWNER);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(outcome.entries_visited, 1);
        assert_eq!(guest.marks.len(), 1);
        assert!(guest.wakes.is_empty());
    }

    #[test]
    fn the_pending_slot_is_handled_last_and_only_once() {
        let mut guest = FakeGuest::default();
        // `list_op_pending` also appears in the list, as it does while glibc is
        // mid-enqueue. The kernel skips it in the loop and handles it at the end.
        guest.head(0x1000, 0x2020, 0x3020);
        guest.node(0x2020, 0x3020, FUTEX_WAITERS | OWNER);
        guest.node(0x3020, 0x1000, FUTEX_WAITERS | OWNER);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(outcome.entries_visited, 2);
        assert_eq!(
            guest.marked_words(),
            vec![0x2000, 0x3000],
            "the pending word must be marked exactly once, after the list"
        );
    }

    #[test]
    fn a_pending_op_on_a_zero_word_wakes_without_marking_it_dead() {
        let mut guest = FakeGuest::default();
        // The owner died between releasing the futex and waking its waiter.
        guest.head(0x1000, 0x1000, 0x3020);
        guest.node(0x3020, 0x1000, 0);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(outcome.entries_visited, 1);
        assert_eq!(guest.wakes, vec![(0x3000, 0)]);
        assert!(
            guest.marks.is_empty(),
            "setting FUTEX_OWNER_DIED on a free word would corrupt user-space state"
        );
        assert_eq!(guest.get_u32(0x3000), Some(0));
    }

    #[test]
    fn a_pending_op_with_no_owner_wakes_with_flag_bits_set() {
        for flags in [
            FUTEX_OWNER_DIED,
            FUTEX_WAITERS,
            FUTEX_OWNER_DIED | FUTEX_WAITERS,
        ] {
            let mut guest = FakeGuest::default();
            guest.head(0x1000, 0x1000, 0x3020);
            guest.node(0x3020, 0x1000, flags);

            let outcome = walk(&mut guest, 0x1000, OWNER);

            assert_eq!(outcome.entries_visited, 1);
            assert_eq!(guest.wakes, vec![(0x3000, flags)]);
            assert!(guest.marks.is_empty());
            assert_eq!(guest.get_u32(0x3000), Some(flags));
        }
    }

    #[test]
    fn a_zero_word_in_the_list_body_is_not_a_pending_op_wake() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x1000, 0);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(outcome.entries_visited, 1);
        assert!(
            guest.wakes.is_empty(),
            "pending_op is false for entries reached through the list body"
        );
        assert!(guest.marks.is_empty());
    }

    #[test]
    fn a_pi_pending_op_on_a_zero_word_does_not_wake() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x1000, 0x3021); // PI bit on the pending slot
        guest.node(0x3020, 0x1000, 0);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(outcome.entries_visited, 1);
        assert!(guest.wakes.is_empty(), "the kernel guards this on `!pi`");
        assert!(guest.marks.is_empty());
    }

    #[test]
    fn the_walk_bound_is_the_kernel_s_literal_robust_list_limit() {
        // `#define ROBUST_LIST_LIMIT 2048` in `kernel/futex/core.c`. Compared
        // against the literal on purpose: a test that compared against
        // `ROBUST_LIST_LIMIT` itself would stay green for any value of it, so
        // it could not tell a faithful bound from a retuned one.
        assert_eq!(ROBUST_LIST_LIMIT, 2048);
    }

    #[test]
    fn a_cycle_is_bounded_by_robust_list_limit() {
        let mut guest = FakeGuest::default();
        // Two nodes pointing at each other, never reaching the head.
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x3020, FUTEX_WAITERS | 7);
        guest.node(0x3020, 0x2020, FUTEX_WAITERS | 7);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert!(outcome.truncated);
        // Also the kernel's literal 2048, for the same reason: the assertion
        // has to fail if the walk follows a different number of entries than
        // Linux does, whatever this crate's constant happens to say.
        assert_eq!(
            outcome.entries_visited, 2048,
            "a cyclic list must be followed for exactly the kernel's 2048 entries"
        );
    }

    // ---- the compare-and-swap retry ---------------------------------------

    #[test]
    fn a_futex_re_owned_between_the_read_and_the_write_is_left_to_its_new_owner() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x1000, FUTEX_WAITERS | OWNER);
        // Thread 7 acquires the mutex after the walk has read the word and
        // before it writes: the window `futex_cmpxchg_value_locked()` exists to
        // detect. A blind write would stamp a live thread's lock
        // FUTEX_OWNER_DIED and tell its waiters the owner is gone.
        guest.race_after_read(0x2000, FUTEX_WAITERS | 7);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(
            guest.marks,
            vec![],
            "thread 7's live mutex must not be stamped FUTEX_OWNER_DIED"
        );
        assert_eq!(
            guest.get_u32(0x2000),
            Some(FUTEX_WAITERS | 7),
            "the word must still be thread 7's"
        );
        assert!(guest.wakes.is_empty());
        assert_eq!(outcome.entries_visited, 1);
        assert_eq!(outcome.cas_retries, 1);
    }

    #[test]
    fn backend_exit_cleanup_does_not_overwrite_a_store_after_the_final_read() {
        let mut guest = FakeGuest {
            defer_owner_death_to_backend: true,
            ..FakeGuest::default()
        };
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x1000, FUTEX_WAITERS | OWNER);
        // Thread 7 acquires the process-shared mutex after Detcore's final
        // read. The old production implementation then performed a separate
        // write and overwrote this live owner. Linux's exit cleanup repeats
        // the comparison atomically and therefore leaves thread 7 untouched.
        guest.race_before_write(0x2000, FUTEX_WAITERS | 7);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(outcome.cas_retries, 0);
        assert_eq!(guest.get_u32(0x2000), Some(FUTEX_WAITERS | 7));
        assert!(guest.marks.is_empty());
        // A stale modeled wake is harmless: the waiter rechecks the live owner
        // after it resumes. The prohibited behavior is overwriting that owner.
        assert_eq!(guest.wakes.len(), 1);
    }

    #[test]
    fn fake_guest_exposes_the_separate_read_write_window() {
        let mut guest = FakeGuest::default();
        guest.put_u32(0x2000, FUTEX_WAITERS | OWNER);
        guest.race_before_write(0x2000, FUTEX_WAITERS | 7);

        let outcome = guest.compare_and_swap(
            0x2000,
            FUTEX_WAITERS | OWNER,
            FUTEX_WAITERS | FUTEX_OWNER_DIED,
        );

        assert_eq!(outcome, FutexCasOutcome::Stored);
        assert_eq!(
            guest.get_u32(0x2000),
            Some(FUTEX_WAITERS | FUTEX_OWNER_DIED),
            "the fake must expose that a separate write overwrites the intervening store"
        );
        assert_eq!(
            guest.marks,
            vec![(
                0x2000,
                FUTEX_WAITERS | OWNER,
                FUTEX_WAITERS | FUTEX_OWNER_DIED
            )]
        );
    }

    #[test]
    fn a_waiter_arriving_between_the_read_and_the_write_is_still_woken() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        // The first read sees an uncontended mutex, so the decision made from
        // it is "mark FUTEX_OWNER_DIED, wake nobody".
        guest.node(0x2020, 0x1000, OWNER);
        // A waiter sets FUTEX_WAITERS inside the window. A blind write would
        // store bare FUTEX_OWNER_DIED over it, erasing FUTEX_WAITERS and
        // issuing no wake, and that waiter would never be woken by anyone.
        guest.race_after_read(0x2000, FUTEX_WAITERS | OWNER);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(
            guest.wakes,
            vec![(0x2000, FUTEX_WAITERS | FUTEX_OWNER_DIED)],
            "the arriving waiter must be woken; the wake decision has to be \
             recomputed from the re-read word"
        );
        assert_eq!(
            guest.marks,
            vec![(
                0x2000,
                FUTEX_WAITERS | OWNER,
                FUTEX_WAITERS | FUTEX_OWNER_DIED
            )],
            "the stored value must be recomputed from the re-read word, so \
             FUTEX_WAITERS survives"
        );
        assert_eq!(
            guest.get_u32(0x2000),
            Some(FUTEX_WAITERS | FUTEX_OWNER_DIED)
        );
        assert_eq!(outcome.cas_retries, 1);
    }

    #[test]
    fn the_walk_retries_until_the_word_stops_moving() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x1000, OWNER);
        // Three stores by the same owner land in three successive windows.
        guest.race_after_read(0x2000, FUTEX_WAITERS | OWNER);
        guest.race_after_read(0x2000, OWNER);
        guest.race_after_read(0x2000, FUTEX_WAITERS | OWNER);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(
            guest.get_u32(0x2000),
            Some(FUTEX_WAITERS | FUTEX_OWNER_DIED)
        );
        assert_eq!(guest.marked_words(), vec![0x2000]);
        assert_eq!(
            outcome.cas_retries, 3,
            "one retry per store that landed inside the window"
        );
    }

    #[test]
    fn a_word_freed_between_the_read_and_the_write_is_not_marked_dead() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x1000, FUTEX_WAITERS | OWNER);
        // The dying thread's own unlock lands in the window. `uval` no longer
        // names it, so Linux returns 0 and leaves the free word alone.
        guest.race_after_read(0x2000, 0);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(
            guest.get_u32(0x2000),
            Some(0),
            "stamping FUTEX_OWNER_DIED on a free word is the corruption the \
             comparison prevents"
        );
        assert!(guest.marks.is_empty() && guest.wakes.is_empty());
        assert_eq!(outcome.cas_retries, 1);
    }

    #[test]
    fn an_uncontended_word_is_still_marked_when_nothing_races() {
        // The bracket's other direction: with no racing store the walk must
        // still perform the transition, so the retry logic cannot be satisfied
        // by simply never writing.
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x1000, OWNER);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert_eq!(outcome.cas_retries, 0);
        assert_eq!(guest.marks, vec![(0x2000, OWNER, FUTEX_OWNER_DIED)]);
        assert!(guest.wakes.is_empty());
    }

    #[test]
    fn an_unreadable_head_aborts_before_any_effect() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x1000, FUTEX_WAITERS | OWNER);
        guest.unreadable.insert(0x1000 + HEAD_FUTEX_OFFSET_OFFSET);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert!(outcome.head_unreadable);
        assert_eq!(outcome.entries_visited, 0);
        assert!(guest.marks.is_empty() && guest.wakes.is_empty());
    }

    #[test]
    fn a_head_with_no_backing_memory_aborts() {
        // The `execve` hazard in miniature: were a stale head ever consulted in
        // a brand new address space, it reads as nothing and the walk must touch
        // nothing. `ThreadState::take_robust_list_for_exec` is what stops the
        // stale head from being consulted at all.
        let mut guest = FakeGuest::default();

        let outcome = walk(&mut guest, 0x7fff_0000, OWNER);

        assert!(outcome.head_unreadable);
        assert!(guest.marks.is_empty() && guest.wakes.is_empty());
    }

    #[test]
    fn a_misaligned_futex_word_is_refused_and_aborts_the_walk() {
        let mut guest = FakeGuest::default();
        // futex_offset -31 puts the word at 0x2001, which Linux rejects before
        // reading it. The later entry must not be reached.
        guest.put_u64(0x1000 + HEAD_LIST_OFFSET, 0x2020);
        guest.put_u64(0x1000 + HEAD_FUTEX_OFFSET_OFFSET, (-31i64) as u64);
        guest.put_u64(0x1000 + HEAD_LIST_OP_PENDING_OFFSET, 0);
        guest.put_u64(0x2020, 0x3020);
        guest.put_u32(0x2001, FUTEX_WAITERS | OWNER);
        guest.put_u64(0x3020, 0x1000);
        guest.put_u32(0x3001, FUTEX_WAITERS | OWNER);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert!(outcome.aborted);
        assert_eq!(outcome.entries_visited, 1);
        assert!(
            guest.marks.is_empty() && guest.wakes.is_empty(),
            "an unaligned word must not be read-modify-written"
        );
    }

    #[test]
    fn a_faulting_futex_word_aborts_the_rest_of_the_walk() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x3020, FUTEX_WAITERS | OWNER);
        guest.node(0x3020, 0x1000, FUTEX_WAITERS | OWNER);
        guest.unreadable.insert(0x2000);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert!(outcome.aborted);
        assert_eq!(outcome.entries_visited, 1);
        assert!(
            guest.marks.is_empty() && guest.wakes.is_empty(),
            "Linux returns from the whole walk; it does not continue fail-open"
        );
    }

    #[test]
    fn an_unwritable_futex_word_aborts_the_rest_of_the_walk() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x3020, FUTEX_WAITERS | OWNER);
        guest.node(0x3020, 0x1000, FUTEX_WAITERS | OWNER);
        guest.unwritable.insert(0x2000);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert!(outcome.aborted);
        assert!(guest.marks.is_empty());
        assert!(guest.wakes.is_empty());
        assert_eq!(guest.get_u32(0x3000), Some(FUTEX_WAITERS | OWNER));
    }

    #[test]
    fn an_abort_also_skips_the_pending_slot() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0x4020);
        guest.node(0x2020, 0x1000, FUTEX_WAITERS | OWNER);
        guest.node(0x4020, 0x1000, FUTEX_WAITERS | OWNER);
        guest.unreadable.insert(0x2000);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert!(outcome.aborted);
        assert!(
            guest.wakes.is_empty() && guest.marks.is_empty(),
            "Linux's bare `return` skips the trailing pending handling too"
        );
    }

    #[test]
    fn a_faulting_next_pointer_still_handles_the_current_entry() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x3020, FUTEX_WAITERS | OWNER);
        guest.unreadable.insert(0x2020);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert!(outcome.next_faulted);
        assert_eq!(outcome.entries_visited, 1);
        assert_eq!(
            guest.marked_words(),
            vec![0x2000],
            "the kernel checks fetch_robust_entry's rc only after handling the entry"
        );
    }
}
