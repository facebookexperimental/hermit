/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Look at total order of clock_gettime across threads

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread;

// use nix::sys::time::TimeSpec;
use nix::sys::time::TimeValLike;
// use nix::time::clock_settime;
use nix::time::ClockId;
// use nix::time::clock_getres;
use nix::time::clock_gettime;

/// Absolute nanoseconds.
type Tm = u128;

/// Return absolute time in nanoseconds.
fn now() -> Tm {
    let clockid = ClockId::CLOCK_MONOTONIC;
    let tmspc = clock_gettime(clockid).unwrap();
    let secs = tmspc.num_seconds();
    // num_nanoseconds actually seems to report absolute time, not just the ns
    // component. There's no obvious way to get just the ns component out of the timespec:
    let ns = tmspc.num_nanoseconds() % 1_000_000_000;
    (secs as Tm) * 1_000_000_000 + (ns as Tm)
}

#[inline(never)]
fn do_work(iters: u64) -> f64 {
    let mut spurious = 1.1;
    for _ in 0..iters {
        spurious /= 0.999999999999;
        spurious += 0.000001;
    }
    spurious
}

fn thread1(flag: Arc<AtomicU64>) -> (Tm, Tm) {
    let tm1 = now();
    do_work(10_000);
    let tm2 = now();
    flag.store(1, Ordering::SeqCst);
    (tm1, tm2)
}

fn thread2(flag: Arc<AtomicU64>) -> (Tm, Tm, Tm) {
    let tm3 = now();
    while flag.load(Ordering::SeqCst) == 0 {
        // wait.
    }
    let tm4 = now();
    do_work(10_000);
    let tm5 = now();
    (tm3, tm4, tm5)
}

fn main() {
    let flag = Arc::new(AtomicU64::new(0));
    let flag2 = flag.clone();
    let h2 = thread::spawn(move || thread2(flag2));
    let h1 = thread::spawn(move || thread1(flag));

    let (tm1, tm2) = h1.join().unwrap();
    let (tm3, tm4, tm5) = h2.join().unwrap();

    println!("First time, anchor: {}", tm1);
    println!("2nd time, relative {}", (tm2 as i128) - (tm1 as i128));
    println!("3nd time, relative {}", (tm3 as i128) - (tm1 as i128));
    println!("4th time, relative {}", (tm4 as i128) - (tm1 as i128));
    println!("5th time, relative {}", (tm5 as i128) - (tm1 as i128));

    assert!(tm1 < tm2);
    assert!(tm3 <= tm4);
    assert!(tm4 <= tm5);

    // Totally ordered ACROSS threads:
    assert!(
        tm2 <= tm4,
        "clock values were not ordered across threads, in spite of causal path"
    );
}
