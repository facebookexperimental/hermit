/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Race a (parent) thread sleeping, with one spinning/printing.
mod test_utils;

use crate::test_utils::sleep_race;

fn run_test() -> u64 {
    sleep_race(|| {
        unsafe {
            let mut tp = libc::timespec {
                tv_sec: 0,
                tv_nsec: 100_000_000, // 100ms is just a random amount of time that makes the test fast to complete.
            };
            // issue a raw clock_gettime syscall to the OS to get the current time
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut tp);
            tp.tv_nsec += 100_000_000; // 100ms is just a random amount of time that makes the test fast to complete.
            if tp.tv_nsec / test_utils::NANOS_PER_SEC > 0 {
                tp.tv_sec += tp.tv_nsec / test_utils::NANOS_PER_SEC;
                tp.tv_nsec %= test_utils::NANOS_PER_SEC;
            }
            // issue a raw clock_nanosleep syscall to the OS
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &tp,
                std::ptr::null_mut(),
            )
        };
    })
}

/// Raw execution exposes nondeterminism with this race.
fn main() {
    let final_count = run_test();
    assert!(final_count > 0); // Safe bet with a 100ms sleep.
}
