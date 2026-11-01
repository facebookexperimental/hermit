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
//! DBT invokes its task-exit callback before executing the native exit. There
//! Detcore also leaves the word to Linux and wakes at once, and the scheduler
//! commits no turn until the kernel has ended the thread
//! (`Scheduler::exited_thread_barriers`), so the waiter runs only after Linux
//! has made the transition. For an exit_group the threads it kills hold their
//! native exits until its walk is done
//! (`ThreadState::wait_for_exit_group_robust_walk`), so Linux marks no word
//! before the walk reads it. SaBRe, which has neither a post-cleanup callback
//! nor that barrier, does not run this path.
//! The fake below models the unsafe separate read/write window so an
//! implementation cannot hide it behind an atomic test double.
//!
//! On KVM no Linux task exits for a guest thread, so no kernel walk would
//! ever mark a word (`BackendCapabilities::kernel_never_walks_robust_lists`).
//! There Detcore makes the transition itself, in the dying thread's own turn,
//! with the guest compare-and-exchange (`MemoryAccess::compare_exchange_u32`),
//! which is atomic with respect to every vCPU that can reach the word, as the
//! kernel's `futex_cmpxchg_value_locked` is; it never stores with a separate
//! read and write. A word the backend cannot exchange atomically (one on file
//! backing it has no atomic store for) stops the walk, and the run is refused
//! by name ([`FutexCasOutcome::Unsupported`]), never left unmarked.
//!
//! # Lifecycle scope
//!
//! Ptrace mirrors every thread's registration and can read all sibling lists
//! before `exit_group` or a Guest-observed fatal signal destroys the address
//! space. It holds those modeled wakes until the physical-exit callback, after
//! Linux has performed the atomic owner-word transition. Fatal signals that
//! terminate an injected blocking syscall do not currently provide that
//! Guest-bearing callback. A successful exec reads the group's lists at its
//! entry and delivers their wakes after it returns
//! (`ThreadState::stage_robust_list_wakes_for_exec`). DBT models an owner's
//! exit, an exit_group and a successful exec. For an exec, Detcore reads the
//! lists before the native exec and the coordinator delivers the wakes when
//! the new image reconnects; an exec that fails before its point of no return
//! drops them (<https://github.com/rrnewton/hermit/issues/4268>). An exec
//! whose target the kernel cannot load (it kills the process with SIGSEGV)
//! hangs a DBT run for any guest, robust mutex or not: DynamoRIO's own exec
//! succeeds and the new image reconnects, which queues the wakes, but that
//! image then dies without an exit hook and is never retired, so no thread
//! gets another turn (<https://github.com/rrnewton/hermit/issues/4362>). The
//! delivery at the caller's deregistration covers an old image that
//! deregisters before any reconnect. Ptrace loses those wakes
//! (<https://github.com/rrnewton/hermit/issues/4363>). DBT does not yet model
//! a fatal signal (<https://github.com/rrnewton/hermit/issues/4278>); SaBRe
//! models none of them. KVM walks, stores and wakes at an owner's own exit; at
//! the grant of the `Exit` request that ends a whole group by exit_group or
//! by a SIGKILL a thread sends its own process; and at a delivery whose
//! default action ends the process, in the delivering thread's turn, which
//! the delivery then leaves without the post-handler hook, so no thread of
//! the dying group runs guest code after the walk. An exec walks and stores
//! at its point of no return, which the backend reports through
//! `Tool::handle_exec_commit` after it has read the strings the exec copies,
//! and holds the wakes until the new image's first callback; an exec that
//! fails walks nothing
//! (<https://github.com/rrnewton/hermit/issues/4529>). KVM refuses an exec by
//! a thread that is not its group's leader, and a signal to another process,
//! so neither of those deaths reaches Detcore there.
//!
//! # Structure
//!
//! [`exit_robust_list`] is the whole algorithm, written against the
//! [`RobustDeathEffects`] trait rather than against a `Guest`, so the walk —
//! ordering, the pending slot, the fault-abort rules and the
//! `ROBUST_LIST_LIMIT` bound — is unit-testable over fake guest memory with no
//! backend, no scheduler and no tracee. `threads.rs` supplies the one real
//! implementation.

use std::collections::BTreeMap;
use std::collections::HashMap;

use serde::Deserialize;
use serde::Serialize;
use tracing::trace;

use crate::types::FutexID;

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

    /// Whether this slot is NULL: an unset `list_op_pending`, or a `next`
    /// pointer of 0 (which Linux then compares with `list_op_pending` as any
    /// other pointer, and faults on when it fetches through it).
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
    /// The exchange Linux would have made cannot be made at all
    /// ([`FutexCasOutcome::Unsupported`]). The walk stops here, untouched
    /// beyond this word, and its caller refuses the run by name.
    Unsupported,
}

/// What one attempted futex-word replacement reported, mirroring the kernel's
/// `futex_cmpxchg_value_locked()` and the `nval != uval` test that follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FutexCasOutcome {
    /// The word still held the expected value, so the replacement goes ahead.
    ///
    /// Where the backend's native task exit makes the transition
    /// (ptrace), the production effects do not store the word themselves: the
    /// native exit repeats the owner check and replaces the word atomically
    /// before the modeled wake can let another thread run; on DBT the
    /// scheduler's exited-thread barrier gives the same order. Later reads in
    /// the same exit see the value Linux will store. Where no native exit
    /// makes it (KVM), the production effects store it with the guest's
    /// atomic compare-and-exchange. The unit-test model stores it immediately,
    /// so the walk can be checked against the kernel's write sequence.
    Matched,
    /// The word held this other value instead, so nothing was written. The
    /// kernel's `goto retry`.
    Changed(u32),
    /// The access faulted and could not be completed. The kernel's `return -1`
    /// after `fault_in_user_writeable()` also fails.
    Faulted,
    /// The backend cannot exchange this word atomically at all (its guest
    /// compare-and-exchange answered a missing capability or a failure of its
    /// own, not a fault), where Linux would have made the exchange. Nothing
    /// was written; the walk stops, and its caller refuses the run by name
    /// rather than leave a word Linux marks unmarked.
    Unsupported,
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
    /// can run may return [`FutexCasOutcome::Matched`] without storing and
    /// leave the atomic replacement to it.
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
    /// The backend could not make an exchange Linux would have made
    /// ([`FutexCasOutcome::Unsupported`]); the walk stopped there.
    pub unsupported: bool,
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
    let pending = RobustEntry::decode(pending_raw);

    let mut limit = ROBUST_LIST_LIMIT;
    while entry.address != head {
        // `fetch_robust_entry(&next_entry, &entry->next, &next_pi)` runs BEFORE
        // this entry is handled, because handling it can rewrite guest memory.
        // `struct robust_list` is a single `next` pointer at offset 0.
        let next = effects.read_u64(entry.address).map(RobustEntry::decode);

        // The kernel skips `pending` here and handles it once at the end. It
        // compares the two as pointers (`if (entry != pending)`, with the PI
        // bit already stripped from both), so a NULL entry equals a NULL
        // `list_op_pending` and is skipped too: Linux leaves the word at
        // `NULL + futex_offset` alone, and the fetch of `NULL->next` above
        // then ends the walk.
        if entry.address != pending.address {
            outcome.entries_visited += 1;
            match handle_futex_death(
                effects,
                entry,
                futex_offset,
                owner_tid,
                false,
                &mut outcome.cas_retries,
            )
            .await
            {
                DeathStep::Continue => {}
                DeathStep::Abort => {
                    outcome.aborted = true;
                    return outcome;
                }
                DeathStep::Unsupported => {
                    outcome.unsupported = true;
                    return outcome;
                }
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
    // it last and with `pending_op` set (`if (pending)`).
    if !pending.is_null() {
        outcome.entries_visited += 1;
        match handle_futex_death(
            effects,
            pending,
            futex_offset,
            owner_tid,
            true,
            &mut outcome.cas_retries,
        )
        .await
        {
            DeathStep::Continue => {}
            DeathStep::Abort => outcome.aborted = true,
            DeathStep::Unsupported => outcome.unsupported = true,
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
            FutexCasOutcome::Matched => {
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
            FutexCasOutcome::Unsupported => return DeathStep::Unsupported,
        }
    }
}

/// A thread's robust list as Linux's `exit_robust_list` reads it when a
/// SIGKILL sent by another thread ends the owner: the registered head and every
/// value the walk reads, by guest address. A read that faulted is absent, and
/// faults again when the walk is replayed over the snapshot.
///
/// A SIGKILL ends its victims without a stop at which Detcore could read their
/// memory, and the exit callback that follows runs after the address space is
/// gone. So the sender records each victim's list before the send
/// ([`record_robust_list`]), in its own turn, while no other guest thread runs
/// and so nothing can change the list before Linux walks it, and the victim's
/// exit callback replays the walk over the record
/// ([`owner_death_wakes_in_snapshot`]) with its own futex identities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobustListSnapshot {
    head: usize,
    pointers: BTreeMap<usize, u64>,
    words: BTreeMap<usize, u32>,
}

/// Read one value of type `V` at `address` in process `pid`'s memory with
/// `process_vm_readv`, which faults where the dying task's own `get_user`
/// would: on an unmapped page or one without read permission.
fn read_remote<V: Copy + Default>(pid: libc::pid_t, address: usize) -> Option<V> {
    let mut value = V::default();
    let size = std::mem::size_of::<V>();
    let local = libc::iovec {
        iov_base: (&mut value as *mut V).cast(),
        iov_len: size,
    };
    let remote = libc::iovec {
        iov_base: address as *mut libc::c_void,
        iov_len: size,
    };
    // SAFETY: one local iovec naming `value`'s `size` bytes; the remote one is
    // only read, by the kernel, which checks it against `pid`'s mappings.
    let read = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
    (read == size as isize).then_some(value)
}

/// The walk's effects while recording a list: every read goes to the victim's
/// memory and is kept, nothing is written and nothing is woken.
struct RecordingReader {
    pid: libc::pid_t,
    snapshot: RobustListSnapshot,
}

impl RobustDeathEffects for RecordingReader {
    fn read_u64(&mut self, address: usize) -> Option<u64> {
        let value = read_remote::<u64>(self.pid, address)?;
        self.snapshot.pointers.insert(address, value);
        Some(value)
    }

    fn read_u32(&mut self, address: usize) -> Option<u32> {
        let value = read_remote::<u32>(self.pid, address)?;
        self.snapshot.words.insert(address, value);
        Some(value)
    }

    fn compare_and_swap(
        &mut self,
        address: usize,
        expected: u32,
        _desired: u32,
    ) -> FutexCasOutcome {
        match self.read_u32(address) {
            None => FutexCasOutcome::Faulted,
            Some(observed) if observed == expected => FutexCasOutcome::Matched,
            Some(observed) => FutexCasOutcome::Changed(observed),
        }
    }

    async fn wake_one(&mut self, _address: usize, _observed: u32) {}
}

/// Record thread `tid`'s robust list as Linux will walk it when the thread
/// dies: the list the kernel holds for it (`get_robust_list(2)`), and every
/// value [`exit_robust_list`] reads from it. `None` if the thread registered
/// none, or registered one of another length, which Detcore's own walks skip
/// too (`Detcore::handle_set_robust_list`).
///
/// Which values the walk reads does not depend on the futex words it finds,
/// only on the list's `next` pointers and on where a read faults, and the
/// replay sees the same pointers and faults, so the replay reads nothing the
/// record lacks.
pub(crate) async fn record_robust_list(tid: i32) -> Option<RobustListSnapshot> {
    let head = registered_robust_list(tid)?;
    let mut reader = RecordingReader {
        pid: tid,
        snapshot: RobustListSnapshot {
            head,
            ..Default::default()
        },
    };
    // The owner TID only decides which words would be marked, not what is
    // read; the replay passes the real one.
    exit_robust_list(&mut reader, head, tid as u32).await;
    Some(reader.snapshot)
}

/// The head of the robust list the kernel holds for thread `tid`
/// (`get_robust_list(2)`), if it holds one of the 64-bit length.
fn registered_robust_list(tid: i32) -> Option<usize> {
    let mut head: *mut libc::c_void = std::ptr::null_mut();
    let mut len: libc::size_t = 0;
    // SAFETY: both out-pointers name live locals of the types the syscall
    // writes.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_get_robust_list,
            tid,
            &mut head as *mut *mut libc::c_void,
            &mut len as *mut libc::size_t,
        )
    };
    (rc == 0 && !head.is_null() && len == ROBUST_LIST_HEAD_LEN).then_some(head as usize)
}

/// The walk's effects while replaying a record: reads come from the record,
/// words are told apart as futexes, so two mappings of one word are one word,
/// and the value Linux stores into a word it marks is what a later read of it
/// sees, as in [`crate::syscalls::threads`]'s guest effects.
struct SnapshotEffects<'a, F> {
    snapshot: &'a RobustListSnapshot,
    futex_id: F,
    marked: HashMap<FutexID, u32>,
    wakes: Vec<FutexID>,
}

impl<F: Fn(usize) -> FutexID> RobustDeathEffects for SnapshotEffects<'_, F> {
    fn read_u64(&mut self, address: usize) -> Option<u64> {
        self.snapshot.pointers.get(&address).copied()
    }

    fn read_u32(&mut self, address: usize) -> Option<u32> {
        let value = *self.snapshot.words.get(&address)?;
        let futex = (self.futex_id)(address);
        Some(self.marked.get(&futex).copied().unwrap_or(value))
    }

    fn compare_and_swap(&mut self, address: usize, expected: u32, desired: u32) -> FutexCasOutcome {
        let Some(observed) = self.read_u32(address) else {
            return FutexCasOutcome::Faulted;
        };
        if observed != expected {
            return FutexCasOutcome::Changed(observed);
        }
        // Linux makes the store itself in the victim's exit; this only
        // remembers it for later reads in the same walk.
        self.marked.insert((self.futex_id)(address), desired);
        FutexCasOutcome::Matched
    }

    async fn wake_one(&mut self, address: usize, _observed: u32) {
        self.wakes.push((self.futex_id)(address));
    }
}

/// Replay Linux's `exit_robust_list` for `owner_tid` over `snapshot`, and
/// return the futexes Linux wakes one waiter on, in the walk's order, with how
/// the walk ended. `futex_id` names a word of the owner's address space as a
/// futex, as the owner's own futex calls name it.
pub(crate) async fn owner_death_wakes_in_snapshot(
    snapshot: &RobustListSnapshot,
    owner_tid: u32,
    futex_id: impl Fn(usize) -> FutexID,
) -> (Vec<FutexID>, WalkOutcome) {
    let mut effects = SnapshotEffects {
        snapshot,
        futex_id,
        marked: HashMap::new(),
        wakes: Vec::new(),
    };
    let outcome = exit_robust_list(&mut effects, snapshot.head, owner_tid).await;
    (effects.wakes, outcome)
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
        /// Addresses the backend cannot exchange atomically at all.
        unexchangeable: BTreeSet<usize>,
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
            if self.unexchangeable.contains(&address) {
                return FutexCasOutcome::Unsupported;
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
                return FutexCasOutcome::Matched;
            }
            self.put_u32(address, desired);
            self.marks.push((address, expected, desired));
            FutexCasOutcome::Matched
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
    fn a_self_cycled_entry_is_marked_and_woken_once() {
        // A node whose next pointer is itself: the walk follows it until
        // ROBUST_LIST_LIMIT, marks the word on the first visit and, seeing no
        // owner on every later one, wakes once, as Linux does.
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0);
        guest.node(0x2020, 0x2020, FUTEX_WAITERS | OWNER);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert!(outcome.truncated);
        assert_eq!(guest.marks.len(), 1);
        assert_eq!(
            guest.wakes,
            vec![(0x2000, FUTEX_WAITERS | FUTEX_OWNER_DIED)]
        );
    }

    #[test]
    fn each_thread_pending_on_an_ownerless_word_wakes_once() {
        // Linux's pending rule wakes once for each dying thread whose pending
        // slot holds a non-PI word with no owner, whatever its other bits:
        // free, or already marked by an earlier owner's death, with or
        // without waiters. Two such threads wake twice.
        for word in [0, FUTEX_OWNER_DIED, FUTEX_WAITERS | FUTEX_OWNER_DIED] {
            let mut guest = FakeGuest::default();
            guest.head(0x1000, 0x1000, 0x2020);
            guest.head(0x3000, 0x3000, 0x2020);
            guest.put_u32(0x2000, word);

            walk(&mut guest, 0x1000, OWNER);
            walk(&mut guest, 0x3000, 7);

            assert!(guest.marks.is_empty(), "word {word:#x}");
            assert_eq!(guest.get_u32(0x2000), Some(word), "word {word:#x}");
            assert_eq!(
                guest.wakes,
                vec![(0x2000, word), (0x2000, word)],
                "word {word:#x}"
            );
        }
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

        assert_eq!(outcome, FutexCasOutcome::Matched);
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

    /// A word the backend cannot exchange atomically stops the walk with
    /// `unsupported` set, after the words before it, with nothing written to
    /// it or past it and the pending slot untouched: the caller refuses the
    /// run rather than leave a word Linux marks unmarked.
    #[test]
    fn an_unexchangeable_word_stops_the_walk_for_a_refusal() {
        let mut guest = FakeGuest::default();
        guest.head(0x1000, 0x2020, 0x5020);
        guest.node(0x2020, 0x3020, OWNER);
        guest.node(0x3020, 0x4020, FUTEX_WAITERS | OWNER);
        guest.node(0x4020, 0x1000, OWNER);
        guest.node(0x5020, 0x1000, OWNER);
        guest.unexchangeable.insert(0x3000);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert!(outcome.unsupported);
        assert!(!outcome.aborted);
        assert_eq!(outcome.entries_visited, 2);
        assert_eq!(guest.marked_words(), vec![0x2000]);
        assert!(guest.wakes.is_empty());
        assert_eq!(guest.get_u32(0x3000), Some(FUTEX_WAITERS | OWNER));
        assert_eq!(guest.get_u32(0x4000), Some(OWNER));
        assert_eq!(guest.get_u32(0x5000), Some(OWNER));
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
    fn a_null_entry_matches_a_null_pending_slot_and_is_skipped() {
        // `list.next` and `list_op_pending` both NULL, and `futex_offset` the
        // absolute address of a word the dying thread owns. Linux compares the
        // entry with `pending` as pointers (`if (entry != pending)`), so the
        // NULL entry is the NULL pending slot and is skipped; the fetch of
        // `NULL->next` faults and ends the walk before `if (pending)`, which
        // would have handled nothing anyway (kernel/futex/core.c v6.17,
        // `exit_robust_list`, L1187-L1197).
        let mut guest = FakeGuest::default();
        guest.put_u64(0x1000 + HEAD_LIST_OFFSET, 0);
        guest.put_u64(0x1000 + HEAD_FUTEX_OFFSET_OFFSET, 0x5000);
        guest.put_u64(0x1000 + HEAD_LIST_OP_PENDING_OFFSET, 0);
        guest.put_u32(0x5000, OWNER);

        let outcome = walk(&mut guest, 0x1000, OWNER);

        assert!(outcome.next_faulted, "{outcome:?}");
        assert_eq!(outcome.entries_visited, 0, "{outcome:?}");
        assert!(
            guest.marks.is_empty() && guest.wakes.is_empty(),
            "Linux leaves the word at NULL + futex_offset alone"
        );
        assert_eq!(guest.get_u32(0x5000), Some(OWNER));
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

    // ---- SIGKILL records ----------------------------------------------------

    fn private_futex(address: usize) -> FutexID {
        FutexID::private(
            crate::types::MmId::initial(crate::types::DetTid::from_raw(1)),
            address,
        )
    }

    /// A record of a list holding `entries`, each `(node, word value)` with
    /// glibc's futex offset, and an optional pending slot.
    fn record(
        head: usize,
        entries: &[(usize, u32)],
        pending: Option<(usize, u32)>,
    ) -> RobustListSnapshot {
        let mut snapshot = RobustListSnapshot {
            head,
            ..Default::default()
        };
        let first = entries.first().map_or(head, |(node, _)| *node);
        snapshot.pointers.insert(head, first as u64);
        snapshot
            .pointers
            .insert(head + 8, GLIBC_FUTEX_OFFSET as u64);
        snapshot
            .pointers
            .insert(head + 16, pending.map_or(0, |(node, _)| node as u64));
        for (i, (node, word)) in entries.iter().enumerate() {
            let next = entries.get(i + 1).map_or(head, |(next, _)| *next);
            snapshot.pointers.insert(*node, next as u64);
            snapshot.words.insert(node - 32, *word);
        }
        if let Some((node, word)) = pending {
            snapshot.words.insert(node - 32, word);
        }
        snapshot
    }

    fn replay(
        snapshot: &RobustListSnapshot,
        futex_id: impl Fn(usize) -> FutexID,
    ) -> (Vec<FutexID>, WalkOutcome) {
        futures::executor::block_on(owner_death_wakes_in_snapshot(snapshot, OWNER, futex_id))
    }

    #[test]
    fn a_record_wakes_each_owned_word_with_waiters_and_an_ownerless_pending_word() {
        let snapshot = record(
            0x1000,
            &[
                (0x2020, FUTEX_WAITERS | OWNER),
                (0x3020, OWNER),
                (0x4020, FUTEX_WAITERS | 99),
            ],
            Some((0x5020, FUTEX_WAITERS)),
        );
        let (wakes, outcome) = replay(&snapshot, private_futex);
        assert_eq!(outcome.entries_visited, 4);
        assert!(!outcome.aborted && !outcome.head_unreadable);
        // Owned with waiters: woken. Owned without: marked only. Another
        // thread's: left alone. The pending slot's ownerless word: woken.
        assert_eq!(wakes, vec![private_futex(0x2000), private_futex(0x5000)]);
    }

    #[test]
    fn two_mappings_of_one_word_in_a_record_are_woken_once() {
        // Linux marks the word through the first entry; through the second it
        // finds the word no longer names the owner and leaves it.
        let snapshot = record(
            0x1000,
            &[
                (0x2020, FUTEX_WAITERS | OWNER),
                (0x3020, FUTEX_WAITERS | OWNER),
            ],
            None,
        );
        let one_word = |_address: usize| private_futex(0x2000);
        let (wakes, outcome) = replay(&snapshot, one_word);
        assert_eq!(outcome.entries_visited, 2);
        assert_eq!(wakes, vec![private_futex(0x2000)]);
    }

    #[test]
    fn a_read_the_record_lacks_faults_as_it_did_when_recorded() {
        let mut snapshot = record(0x1000, &[(0x2020, FUTEX_WAITERS | OWNER)], None);
        snapshot.words.clear();
        let (wakes, outcome) = replay(&snapshot, private_futex);
        assert!(outcome.aborted);
        assert!(wakes.is_empty());

        let unregistered = RobustListSnapshot::default();
        let (wakes, outcome) = replay(&unregistered, private_futex);
        assert!(outcome.head_unreadable);
        assert!(wakes.is_empty());
    }

    /// The recorder reads a real glibc list, the one the kernel holds for the
    /// thread, and the replay of the record finds the locked robust mutex.
    #[test]
    fn the_recorder_reads_the_list_the_kernel_holds_for_a_thread() {
        std::thread::spawn(|| {
            // SAFETY: a zeroed mutex and attribute, initialized before use and
            // destroyed after, all on this thread.
            unsafe {
                let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
                let mut mutex: libc::pthread_mutex_t = std::mem::zeroed();
                assert_eq!(libc::pthread_mutexattr_init(&mut attr), 0);
                assert_eq!(
                    libc::pthread_mutexattr_setrobust(&mut attr, libc::PTHREAD_MUTEX_ROBUST),
                    0
                );
                assert_eq!(libc::pthread_mutex_init(&mut mutex, &attr), 0);
                assert_eq!(libc::pthread_mutex_lock(&mut mutex), 0);
                let word = &mut mutex as *mut libc::pthread_mutex_t as usize;
                let tid = libc::gettid();
                // A waiter's bit, so Linux's walk would wake one.
                let lock = &*(word as *const std::sync::atomic::AtomicU32);
                lock.fetch_or(FUTEX_WAITERS, std::sync::atomic::Ordering::SeqCst);

                let snapshot = futures::executor::block_on(record_robust_list(tid))
                    .expect("glibc registers every thread's robust list");
                assert_eq!(
                    snapshot.words.get(&word),
                    Some(&(FUTEX_WAITERS | tid as u32))
                );
                let (wakes, outcome) = futures::executor::block_on(owner_death_wakes_in_snapshot(
                    &snapshot,
                    tid as u32,
                    private_futex,
                ));
                assert_eq!(outcome.entries_visited, 1);
                assert_eq!(wakes, vec![private_futex(word)]);

                lock.fetch_and(!FUTEX_WAITERS, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(libc::pthread_mutex_unlock(&mut mutex), 0);
                assert_eq!(libc::pthread_mutex_destroy(&mut mutex), 0);
                assert_eq!(libc::pthread_mutexattr_destroy(&mut attr), 0);
            }
        })
        .join()
        .unwrap();
    }
}
