/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*! One of the simplest possible race conditions.
    Exits with a nonzero code under one order, and not the other.

```cargo
 [dependencies]
 core = "1.6.0"
```
 */

use core::arch::x86_64::_rdtsc;
use std::ptr::read_volatile;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;

#[inline(never)]
fn rdtsc() -> u64 {
    unsafe { _rdtsc() }
}

const WORK_AMT: usize = 100;

#[inline(never)]
fn do_work(iters: usize) {
    let mut x = iters;
    let mut max = 0;
    while unsafe { read_volatile(&x as *const _) } > 0usize {
        let new = rdtsc();
        max = std::cmp::max(max, new);
        x -= 1;
    }
}

#[inline(never)]
fn thread1(var: Arc<AtomicUsize>) {
    do_work(4 * WORK_AMT);
    var.store(1, SeqCst);
    do_work(4 * WORK_AMT);
}

#[inline(never)]
fn thread2(var: Arc<AtomicUsize>) {
    do_work(4 * WORK_AMT);
    var.store(2, SeqCst);
    do_work(4 * WORK_AMT);
}

fn main() {
    let d = Arc::new(AtomicUsize::new(1));
    let d1 = Arc::clone(&d);
    let d2 = Arc::clone(&d);

    let h1 = std::thread::spawn(move || thread1(d1));
    let h2 = std::thread::spawn(move || thread2(d2));
    h1.join().unwrap();
    h2.join().unwrap();

    let val = Arc::try_unwrap(d).unwrap().into_inner();
    println!("Final value: {}", val);
    if val == 1 {
        println!("Antagonistic schedule reached, failing.");
        std::process::exit(1);
    }
    println!("Did not find antagonistic schedule. Succeeding.");
}
