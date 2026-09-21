/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Several threads each blocked on a DISTINCT futex address at the same time.
//!
//! WHY A DISTINCT ADDRESS PER THREAD, rather than several waiters on one futex.
//! `Scheduler::full_summary` prints `blocked.futex_waiters`, which is a
//! `HashMap<FutexID, Vec<FutexWaiter>>`. A HashMap with ONE key has exactly one
//! iteration order, so a dump taken while a single futex holds waiters cannot
//! show an ordering problem however many threads wait on it -- the number of
//! KEYS is what makes iteration order observable, not the number of waiters.
//!
//! Measured on this host, digest of the iteration order over 5 runs of the same
//! binary: 1 key gave 1 distinct order in 5, 2 keys gave 2, 3 keys gave 3, and
//! 4 and 8 keys gave 5 distinct in 5. Two keys is the minimum that can vary but
//! only admits two permutations, so a short sample misses the variation
//! (probability 2*(1/2)^k, i.e. 25% at k=3 runs). Six is used here so a handful
//! of runs is decisive rather than probabilistic.
//!
//! The guest wakes every waiter and joins, so it terminates normally when run
//! without `--stop-after-turn`.

use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

const FUTEXES: usize = 6;

static SLOTS: [AtomicU32; FUTEXES] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

fn main() {
    let mut handles = Vec::with_capacity(FUTEXES);
    for slot in SLOTS.iter() {
        handles.push(std::thread::spawn(move || {
            // FUTEX_WAIT with the expected value equal to the current value, so
            // the thread parks rather than returning EAGAIN. FUTEX_PRIVATE_FLAG
            // keys the futex by address space and address, which is the
            // `FutexID::Private` case the summary prints.
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    slot as *const AtomicU32,
                    libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                    0u32,
                    std::ptr::null::<libc::timespec>(),
                );
            }
        }));
    }

    // Give every child time to reach FUTEX_WAIT before any wake is issued, so a
    // dump taken in this window sees all six keys populated.
    std::thread::sleep(std::time::Duration::from_millis(200));

    for slot in SLOTS.iter() {
        slot.store(1, Ordering::SeqCst);
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                slot as *const AtomicU32,
                libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                i32::MAX,
            );
        }
    }

    for handle in handles {
        handle.join().expect("child thread panicked");
    }
    println!("multi_futex_block: {} futexes woken", FUTEXES);
}
