/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use core::ops::Add;

use nix::sys::time::TimeSpec;
use nix::sys::time::TimeValLike;
use nix::time::clock_getres;
use nix::time::clock_gettime;
use nix::time::clock_settime;
use nix::time::ClockId;

fn main() {
    let clockids: [(&str, ClockId); 10] = [
        ("CLOCK_BOOTTIME", ClockId::CLOCK_BOOTTIME),
        ("CLOCK_BOOTTIME_ALARM", ClockId::CLOCK_BOOTTIME_ALARM),
        ("CLOCK_MONOTONIC", ClockId::CLOCK_MONOTONIC),
        ("CLOCK_MONOTONIC_COARSE", ClockId::CLOCK_MONOTONIC_COARSE),
        ("CLOCK_MONOTONIC_RAW", ClockId::CLOCK_MONOTONIC_RAW),
        (
            "CLOCK_PROCESS_CPUTIME_ID",
            ClockId::CLOCK_PROCESS_CPUTIME_ID,
        ),
        ("CLOCK_REALTIME", ClockId::CLOCK_REALTIME),
        ("CLOCK_REALTIME_ALARM", ClockId::CLOCK_REALTIME_ALARM),
        ("CLOCK_REALTIME_COARSE", ClockId::CLOCK_REALTIME_COARSE),
        // ("CLOCK_TAI", ClockId::CLOCK_TAI),
        ("CLOCK_THREAD_CPUTIME_ID", ClockId::CLOCK_THREAD_CPUTIME_ID),
    ];

    for (name, clockid) in &clockids {
        println!("\nTesting with clock_id {} ({})", name, clockid);
        let t1 = clock_gettime(*clockid).unwrap();
        println!("  T1: {}", t1);
        let t2 = clock_gettime(*clockid).unwrap();
        println!("  T2: {}", t2);
        let res = clock_getres(*clockid).unwrap();
        println!("  res: {}", res);

        if res <= TimeSpec::nanoseconds(1) {
            assert!(t2 > t1); // It takes more than a nanosecond to print.
        } else {
            assert!(t2 >= t1);
        }

        let one_sec = TimeSpec::microseconds(1_000_000);
        let t3 = t2.add(one_sec);
        let error = clock_settime(*clockid, t3).is_err();
        if error {
            eprintln!("  Error: could not settime with clockID {}...", clockid);
        }
        let t4 = clock_gettime(*clockid).unwrap();
        let dur: TimeSpec = t4 - t2;
        if error {
            println!("  third measurement: {}, difference {}", t4, dur);
            if res > TimeSpec::nanoseconds(1) {
                assert!(dur >= TimeSpec::nanoseconds(0));
            } else {
                assert!(dur > TimeSpec::nanoseconds(0));
            }
        } else {
            println!("  after update: {}, difference {}", t4, dur);
            assert!(dur >= one_sec);
        }
    }

    println!("Success.");
}
