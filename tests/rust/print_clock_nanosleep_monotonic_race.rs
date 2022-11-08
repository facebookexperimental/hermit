// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! Race a (parent) thread sleeping, with one spinning/printing.
mod test_utils;

use crate::test_utils::sleep_race;

fn run_test() -> u64 {
    sleep_race(|| {
        unsafe {
            let tp = libc::timespec {
                tv_sec: 0,
                tv_nsec: 100_000_000, // 100ms is just a random amount of time that makes the test fast to complete.
            };
            // issue a raw clock_nanosleep syscall to the OS
            libc::clock_nanosleep(libc::CLOCK_MONOTONIC, 0, &tp, std::ptr::null_mut())
        };
    })
}

/// Raw execution exposes nondeterminism with this race.
fn main() {
    let final_count = run_test();
    assert!(final_count > 0); // Safe bet with a 100ms sleep.
}
