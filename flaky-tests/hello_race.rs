/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*! One of the simplest possible race conditions.
    Exits with a nonzero code under one order, and not the other.

    However, we complicate things a bit by having an rdtsc and a branch right before
    the critical event, to make it easier to recognize in the raw schedule recordings,
    as when manually checking the results of this test.

```cargo
 [dependencies]
 core = "1.6.0"
```
 */

use core::arch::x86_64::_rdtsc;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;

#[inline(always)]
fn rdtsc() -> u64 {
    unsafe { _rdtsc() }
}

const WORK_AMT: usize = 100;

#[inline(never)]
fn do_work(iters: usize) -> u64 {
    let mut max = 0;
    for _i in 0..iters {
        let new = rdtsc();
        max = std::cmp::max(max, new);
    }
    max
}

#[inline(never)]
fn thread1(var: Arc<AtomicUsize>) {
    do_work(4 * WORK_AMT);
    // Provide a little extra context so we can judge the accuracy of the pinpointing result.
    let fin = rdtsc(); // last intercepted instruction
    if fin.is_multiple_of(2)
    // last branch before critical event
    {
        var.store(1, SeqCst); // critical event
    } else {
        var.store(11, SeqCst); // critical event
    }
    do_work(4 * WORK_AMT);
}

#[inline(never)]
fn thread2(var: Arc<AtomicUsize>) {
    do_work(4 * WORK_AMT);

    // Provide a little extra context so we can judge the accuracy of the pinpointing result.
    let fin = rdtsc(); // last intercepted instruction
    if fin.is_multiple_of(2)
    // last branch before critical event
    {
        var.store(2, SeqCst); // critical event
    } else {
        var.store(22, SeqCst); // critical event
    }

    do_work(4 * WORK_AMT);
}

#[test]
fn unit_test() {
    run_test();
}

fn run_test() {
    let d = Arc::new(AtomicUsize::new(1));
    let d1 = Arc::clone(&d);
    let d2 = Arc::clone(&d);

    let h1 = std::thread::spawn(move || thread1(d1));
    let h2 = std::thread::spawn(move || thread2(d2));
    h1.join().unwrap();
    h2.join().unwrap();

    let val = Arc::try_unwrap(d).unwrap().into_inner();
    println!("Final value: {}", val);
    if val == 1 || val == 11 {
        println!("Antagonistic schedule reached, failing.");
        std::process::exit(1);
    }
    println!("Did not find antagonistic schedule. Succeeding.");
}

fn main() {
    run_test();
}
