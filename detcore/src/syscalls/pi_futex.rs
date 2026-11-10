/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Linux's priority-inheritance futex protocol (FUTEX_LOCK_PI, FUTEX_LOCK_PI2,
//! FUTEX_TRYLOCK_PI and FUTEX_UNLOCK_PI), as decisions over the futex word and
//! what the scheduler holds for it.
//!
//! The word holds its owner's TID (`FUTEX_TID_MASK`), `FUTEX_WAITERS` once a
//! waiter has queued in the kernel, and `FUTEX_OWNER_DIED` after a robust
//! owner died (Documentation/locking/pi-futex.rst). User space takes and
//! releases an uncontended lock with its own compare-and-exchange; the kernel
//! sees only the slow paths. All line references are to Linux v6.16:
//!
//! - `futex_lock_pi_atomic` (kernel/futex/pi.c:515-609,
//!   <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/futex/pi.c?h=v6.16#n515>)
//!   decides LOCK_PI and TRYLOCK_PI ([`lock_pi_step`]);
//! - `futex_unlock_pi` (pi.c:1132-1293,
//!   <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/futex/pi.c?h=v6.16#n1132>)
//!   and `wake_futex_pi` (pi.c:614-669) decide UNLOCK_PI ([`unlock_pi_step`]);
//! - `fixup_pi_owner` and `__fixup_pi_state_owner` (pi.c:872-907, 671-841)
//!   give a waiter the word after the owner died ([`owner_death_word`]).
//!
//! Detcore keeps no rt_mutex. The scheduler parks LOCK_PI waiters in its futex
//! waiter pool, marked as PI waiters, and records each contended word's owner
//! (`Scheduler::pi_futex_state`). Priority boosting has no meaning under
//! Detcore's scheduler, and waiter order needs none: `__waiter_prio`
//! (kernel/locking/rtmutex.c:349-357) gives every task that is not real-time
//! or deadline the same rt_mutex priority, `DEFAULT_PRIO`, so their waiters
//! are served first in, first out, and `rt_mutex_steal` (rtmutex.c:430-448)
//! lets no equal-priority task take the lock from the top waiter. Detcore
//! accepts `sched_setscheduler` as a no-op, so every guest thread is such a
//! task in the kernel's eyes, and first in, first out is Linux's own order.

use reverie::syscalls::Errno;
use serde::Deserialize;
use serde::Serialize;

use super::robust_list::FUTEX_OWNER_DIED;
use super::robust_list::FUTEX_TID_MASK;
use super::robust_list::FUTEX_WAITERS;
use crate::types::DetTid;

/// How a FUTEX_LOCK_PI waiter was handed the lock it waited for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Serialize, Deserialize)]
pub enum PiHandOver {
    /// FUTEX_UNLOCK_PI, which already wrote the waiter's TID (`wake_futex_pi`).
    Unlock,
    /// The owner's exit or exec (`exit_pi_state_list`), after which the waiter
    /// writes its TID itself ([`owner_death_word`]).
    OwnerDeath,
}

/// What the scheduler holds for a futex word a PI command acts on: Linux's
/// `futex_top_waiter` and the `pi_state` attached to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Serialize, Deserialize)]
pub enum PiFutexState {
    /// No waiter is queued on the word, so the kernel holds no state for it.
    /// Carries whether the TID the word names is a live guest thread, as
    /// `find_get_task_by_vpid` decides in `attach_to_pi_owner` (pi.c:427).
    Unqueued {
        /// The word's owner TID names a live thread.
        named_owner_alive: bool,
        /// Queueing behind that thread would close a cycle of PI waits.
        queueing_deadlocks: bool,
    },
    /// The first waiter is a plain FUTEX_WAIT waiter, which has no `pi_state`
    /// (`attach_to_pi_state` [3], pi.c:214-215; `futex_unlock_pi`,
    /// pi.c:1169-1171).
    PlainWaiters,
    /// A `pi_state` is live: PI waiters are queued, or its owner was handed the
    /// lock and has not yet taken it. Carries the owner and the PI waiter next
    /// in line (`rt_mutex_top_waiter`), if one is queued.
    Contended {
        /// The `pi_state` owner.
        owner: DetTid,
        /// The longest-waiting PI waiter.
        next_owner: Option<DetTid>,
        /// Queueing behind the owner would close a cycle of PI waits.
        queueing_deadlocks: bool,
    },
}

/// What FUTEX_LOCK_PI or FUTEX_TRYLOCK_PI does next ([`lock_pi_step`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LockPiStep {
    /// Return `errno`, after writing `write` to the word when it is set: a
    /// first waiter sets `FUTEX_WAITERS` before it looks the owner up
    /// (pi.c:599-608), and the bit stays when that fails.
    Fail { errno: Errno, write: Option<u32> },
    /// Take the lock: write this word and return 0.
    Acquire(u32),
    /// Queue behind `owner`, after writing `write` to the word when it is set.
    Block { owner: DetTid, write: Option<u32> },
}

/// What FUTEX_UNLOCK_PI does once the caller is known to own the word
/// ([`unlock_pi_step`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnlockPiStep {
    /// Return this errno, leaving the word as it is.
    Fail(Errno),
    /// No waiter is queued: write 0 and return 0 (pi.c:1251-1276).
    Release,
    /// Write `write`, hand the lock to `to` and wake it (`wake_futex_pi`).
    HandOff { to: DetTid, write: u32 },
}

/// The owner TID a futex word names.
pub(crate) fn word_owner(word: u32) -> u32 {
    word & FUTEX_TID_MASK
}

/// The TID a guest thread puts in a PI futex word, as the kernel's
/// `task_pid_vnr` reports it. `DetTid` is the raw namespaced TID
/// (`Detcore::run_robust_list_owner_death` explains why, and names the test
/// that fails the moment the two diverge).
pub(crate) fn guest_tid(thread: DetTid) -> u32 {
    thread.as_raw() as u32
}

/// `futex_lock_pi_atomic` (pi.c:515-609) for `caller` on a word holding `word`,
/// with what the scheduler holds for it, and `futex_lock_pi`'s TRYLOCK_PI
/// answer when `trylock` (pi.c:989-993):
///
/// 1. the caller already owns the word: `EDEADLK` (pi.c:539-540);
/// 2. a waiter is queued (`attach_to_pi_state`, pi.c:203-319): `EINVAL` if it
///    is a plain waiter; otherwise the word must name the state's owner, or be
///    marked `FUTEX_OWNER_DIED` with no owner TID (the owner died and the waiter
///    handed the lock has not yet written its TID, [4]/[6]), else `EINVAL`
///    ([5], [7], [9/10]). Then the caller queues without touching the word;
/// 3. no waiter and no owner TID: take the lock, keeping `FUTEX_OWNER_DIED`
///    (pi.c:559-592);
/// 4. no waiter and an owner TID: set `FUTEX_WAITERS` (pi.c:599-602), then
///    `ESRCH` if the TID names no live thread (`handle_exit_race`, pi.c:374),
///    else queue behind it (`attach_to_pi_owner`).
///
/// A LOCK_PI that would queue behind an owner who waits, through PI futexes,
/// for a lock the caller owns fails with `EDEADLK` instead, before it sleeps
/// (`__rt_mutex_start_proxy_lock`'s chain walk, kernel/locking/rtmutex.c:860-861),
/// leaving the `FUTEX_WAITERS` a first waiter set.
///
/// A TRYLOCK_PI that would queue fails with `EWOULDBLOCK` instead: the
/// rt_mutex is owned, or its top waiter is someone else and no equal-priority
/// task steals it (`rt_mutex_steal`). The `FUTEX_WAITERS` a first waiter set
/// stays in the word.
///
/// The kernel's compare-and-exchange retries (`-EAGAIN` from
/// `lock_pi_update_atomic`) cannot happen here: Detcore runs one guest thread
/// at a time and the caller holds the turn, so the word cannot change between
/// the read and the write. Nor can an owner be found mid-exit (`-EBUSY`): a
/// thread is either still scheduled or already retired.
pub(crate) fn lock_pi_step(
    word: u32,
    caller: u32,
    state: PiFutexState,
    trylock: bool,
) -> LockPiStep {
    let fail = |errno| LockPiStep::Fail { errno, write: None };
    if word_owner(word) == caller {
        return fail(Errno::EDEADLK);
    }
    match state {
        PiFutexState::PlainWaiters => fail(Errno::EINVAL),
        PiFutexState::Contended {
            owner,
            queueing_deadlocks,
            ..
        } => {
            let named = word_owner(word);
            let owner_died_unfixed = word & FUTEX_OWNER_DIED != 0 && named == 0;
            if !owner_died_unfixed && named != guest_tid(owner) {
                fail(Errno::EINVAL)
            } else if trylock {
                fail(Errno::EWOULDBLOCK)
            } else if queueing_deadlocks {
                fail(Errno::EDEADLK)
            } else {
                LockPiStep::Block { owner, write: None }
            }
        }
        PiFutexState::Unqueued { .. } if word_owner(word) == 0 => {
            LockPiStep::Acquire((word & FUTEX_OWNER_DIED) | caller)
        }
        PiFutexState::Unqueued {
            named_owner_alive,
            queueing_deadlocks,
        } => {
            let write = Some(word | FUTEX_WAITERS);
            if !named_owner_alive {
                LockPiStep::Fail {
                    errno: Errno::ESRCH,
                    write,
                }
            } else if trylock {
                LockPiStep::Fail {
                    errno: Errno::EWOULDBLOCK,
                    write,
                }
            } else if queueing_deadlocks {
                LockPiStep::Fail {
                    errno: Errno::EDEADLK,
                    write,
                }
            } else {
                let owner = DetTid::from_raw(word_owner(word) as i32);
                LockPiStep::Block { owner, write }
            }
        }
    }
}

/// `futex_unlock_pi` (pi.c:1132-1293) for `caller`, whose TID the word
/// already names (the `EPERM` check, pi.c:1148-1149, comes first):
///
/// - a plain first waiter, or a state the caller does not own, is `EINVAL`
///   (pi.c:1169-1178);
/// - with PI waiters, `wake_futex_pi` (pi.c:614-669) writes
///   `FUTEX_WAITERS | next owner` (pi.c:631), dropping `FUTEX_OWNER_DIED`,
///   and hands the lock over;
/// - with no waiter the word becomes 0, keeping neither `FUTEX_WAITERS` nor
///   `FUTEX_OWNER_DIED` (pi.c:1251-1258).
pub(crate) fn unlock_pi_step(caller: DetTid, state: PiFutexState) -> UnlockPiStep {
    match state {
        PiFutexState::PlainWaiters => UnlockPiStep::Fail(Errno::EINVAL),
        PiFutexState::Contended { owner, .. } if owner != caller => {
            UnlockPiStep::Fail(Errno::EINVAL)
        }
        PiFutexState::Contended {
            next_owner: Some(to),
            ..
        } => UnlockPiStep::HandOff {
            to,
            write: FUTEX_WAITERS | guest_tid(to),
        },
        PiFutexState::Contended {
            next_owner: None, ..
        }
        | PiFutexState::Unqueued { .. } => UnlockPiStep::Release,
    }
}

/// The word a PI waiter writes when its owner's death handed it the lock, as
/// `fixup_pi_owner` (pi.c:872-886) and `__fixup_pi_state_owner` (pi.c:748-758)
/// write it.
///
/// `exit_pi_state_list` (kernel/futex/core.c:1323-1402) clears the state's
/// owner and leaves the word as the owner's exit made it: `FUTEX_OWNER_DIED`
/// with no TID after a robust list walk, or the dead owner's TID. The waiter
/// then writes its own TID with `FUTEX_WAITERS` and, because the state had no
/// owner, `FUTEX_OWNER_DIED`, robust mutex or not, so glibc reports
/// `EOWNERDEAD` for a robust mutex. A waiter FUTEX_UNLOCK_PI handed the lock
/// writes nothing: the unlock already wrote its TID (pi.c:631).
pub(crate) fn owner_death_word(waiter: u32) -> u32 {
    FUTEX_OWNER_DIED | FUTEX_WAITERS | waiter
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u32 = 7;
    const OWNER: u32 = 5;

    fn tid(raw: u32) -> DetTid {
        DetTid::from_raw(raw as i32)
    }

    fn unqueued(alive: bool) -> PiFutexState {
        PiFutexState::Unqueued {
            named_owner_alive: alive,
            queueing_deadlocks: false,
        }
    }

    fn contended(owner: u32, next: u32) -> PiFutexState {
        PiFutexState::Contended {
            owner: tid(owner),
            next_owner: Some(tid(next)),
            queueing_deadlocks: false,
        }
    }

    #[test]
    fn an_unowned_word_is_taken_keeping_only_owner_died() {
        for trylock in [false, true] {
            assert_eq!(
                lock_pi_step(0, ME, unqueued(false), trylock),
                LockPiStep::Acquire(ME)
            );
            // The waiters bit with no owner and no waiter (the owner unlocked
            // after its last waiter timed out) is not kept.
            assert_eq!(
                lock_pi_step(FUTEX_WAITERS, ME, unqueued(false), trylock),
                LockPiStep::Acquire(ME)
            );
            // A robust owner died with no waiter: glibc then sees
            // FUTEX_OWNER_DIED and reports EOWNERDEAD.
            assert_eq!(
                lock_pi_step(FUTEX_OWNER_DIED, ME, unqueued(false), trylock),
                LockPiStep::Acquire(FUTEX_OWNER_DIED | ME)
            );
        }
    }

    #[test]
    fn the_owner_locking_again_is_a_deadlock_whatever_else_holds() {
        for word in [ME, ME | FUTEX_WAITERS, ME | FUTEX_OWNER_DIED] {
            for state in [unqueued(true), PiFutexState::PlainWaiters, contended(ME, 9)] {
                assert_eq!(
                    lock_pi_step(word, ME, state, false),
                    LockPiStep::Fail {
                        errno: Errno::EDEADLK,
                        write: None
                    }
                );
            }
        }
    }

    #[test]
    fn a_first_waiter_sets_the_waiters_bit_and_queues_behind_the_named_owner() {
        assert_eq!(
            lock_pi_step(OWNER, ME, unqueued(true), false),
            LockPiStep::Block {
                owner: tid(OWNER),
                write: Some(FUTEX_WAITERS | OWNER)
            }
        );
        // TRYLOCK_PI leaves the bit it set.
        assert_eq!(
            lock_pi_step(OWNER, ME, unqueued(true), true),
            LockPiStep::Fail {
                errno: Errno::EWOULDBLOCK,
                write: Some(FUTEX_WAITERS | OWNER)
            }
        );
    }

    #[test]
    fn a_dead_owner_tid_is_esrch_after_the_waiters_bit_is_set() {
        for trylock in [false, true] {
            assert_eq!(
                lock_pi_step(OWNER, ME, unqueued(false), trylock),
                LockPiStep::Fail {
                    errno: Errno::ESRCH,
                    write: Some(FUTEX_WAITERS | OWNER)
                }
            );
        }
    }

    #[test]
    fn later_waiters_queue_behind_the_state_owner_without_writing() {
        assert_eq!(
            lock_pi_step(FUTEX_WAITERS | OWNER, ME, contended(OWNER, 9), false),
            LockPiStep::Block {
                owner: tid(OWNER),
                write: None
            }
        );
        assert_eq!(
            lock_pi_step(FUTEX_WAITERS | OWNER, ME, contended(OWNER, 9), true),
            LockPiStep::Fail {
                errno: Errno::EWOULDBLOCK,
                write: None
            }
        );
        // The owner died robustly and its successor has not run yet ([6]).
        assert_eq!(
            lock_pi_step(FUTEX_WAITERS | FUTEX_OWNER_DIED, ME, contended(9, 9), false),
            LockPiStep::Block {
                owner: tid(9),
                write: None
            }
        );
    }

    #[test]
    fn a_word_that_disagrees_with_the_queued_state_is_einval() {
        let einval = LockPiStep::Fail {
            errno: Errno::EINVAL,
            write: None,
        };
        // Plain waiters have no pi_state ([3]).
        assert_eq!(
            lock_pi_step(OWNER, ME, PiFutexState::PlainWaiters, false),
            einval
        );
        // User space rewrote the owner ([9/10]).
        assert_eq!(
            lock_pi_step(FUTEX_WAITERS | 11, ME, contended(OWNER, 9), false),
            einval
        );
        // A non-robust owner died and its successor has not written its TID:
        // the word still names the dead owner ([7]).
        assert_eq!(
            lock_pi_step(FUTEX_WAITERS | OWNER, ME, contended(9, 9), false),
            einval
        );
        // A TID with FUTEX_OWNER_DIED that is not the owner's ([5]).
        assert_eq!(
            lock_pi_step(FUTEX_OWNER_DIED | 11, ME, contended(OWNER, 9), true),
            einval
        );
    }

    #[test]
    fn unlock_hands_the_lock_to_the_next_owner_or_clears_the_word() {
        assert_eq!(
            unlock_pi_step(tid(ME), unqueued(true)),
            UnlockPiStep::Release
        );
        assert_eq!(
            unlock_pi_step(tid(ME), contended(ME, 9)),
            UnlockPiStep::HandOff {
                to: tid(9),
                write: FUTEX_WAITERS | 9
            }
        );
        // The caller was handed the lock and no waiter is left behind it.
        assert_eq!(
            unlock_pi_step(
                tid(ME),
                PiFutexState::Contended {
                    owner: tid(ME),
                    next_owner: None,
                    queueing_deadlocks: false,
                }
            ),
            UnlockPiStep::Release
        );
        assert_eq!(
            unlock_pi_step(tid(ME), contended(OWNER, 9)),
            UnlockPiStep::Fail(Errno::EINVAL)
        );
        assert_eq!(
            unlock_pi_step(tid(ME), PiFutexState::PlainWaiters),
            UnlockPiStep::Fail(Errno::EINVAL)
        );
    }

    #[test]
    fn a_waiter_handed_the_lock_by_its_owners_death_writes_its_tid_with_owner_died() {
        assert_eq!(owner_death_word(ME), FUTEX_WAITERS | FUTEX_OWNER_DIED | ME);
    }

    #[test]
    fn a_lock_that_would_close_a_cycle_is_a_deadlock_but_a_trylock_is_not() {
        let deadlocking = PiFutexState::Contended {
            owner: tid(OWNER),
            next_owner: Some(tid(9)),
            queueing_deadlocks: true,
        };
        assert_eq!(
            lock_pi_step(FUTEX_WAITERS | OWNER, ME, deadlocking, false),
            LockPiStep::Fail {
                errno: Errno::EDEADLK,
                write: None
            }
        );
        assert_eq!(
            lock_pi_step(FUTEX_WAITERS | OWNER, ME, deadlocking, true),
            LockPiStep::Fail {
                errno: Errno::EWOULDBLOCK,
                write: None
            }
        );
        // A first waiter has already set FUTEX_WAITERS when the walk refuses it.
        let deadlocking = PiFutexState::Unqueued {
            named_owner_alive: true,
            queueing_deadlocks: true,
        };
        assert_eq!(
            lock_pi_step(OWNER, ME, deadlocking, false),
            LockPiStep::Fail {
                errno: Errno::EDEADLK,
                write: Some(FUTEX_WAITERS | OWNER)
            }
        );
    }
}
