/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! This explicitly checks that many schedules are explored. It only fails
//! if a specific interleaving occurs via a CAS handoff.

use std::ptr::read_volatile;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;

const WORK_AMT: usize = 10000;
const FINAL_VALUE: usize = 4;

#[inline(never)]
fn do_work(iters: usize) {
    let mut x = iters;
    while unsafe { read_volatile(&x as *const _) } > 0usize {
        x -= 1;
    }
}

#[inline(never)]
fn take_from(a: &AtomicUsize, from: usize, to: usize) {
    a.compare_exchange(from, to, SeqCst, SeqCst).ok();
}

#[inline(never)]
fn thread1(var: Arc<AtomicUsize>) {
    do_work(5 * WORK_AMT);
    take_from(&var, 2, 3);
}

#[inline(never)]
fn thread2(var: Arc<AtomicUsize>) {
    take_from(&var, 1, 2);
    do_work(25 * WORK_AMT); // <- preemption must occur here, not just starvation
    take_from(&var, 3, FINAL_VALUE);
}

fn main() {
    let d = Arc::new(AtomicUsize::new(1));

    let d1 = Arc::clone(&d);
    let d2 = Arc::clone(&d);

    const FINAL_VALUE: usize = 4;

    let h1 = std::thread::spawn(move || thread1(d1));
    let h2 = std::thread::spawn(move || thread2(d2));

    h1.join().unwrap();
    h2.join().unwrap();

    let val = Arc::try_unwrap(d).unwrap().into_inner();
    println!("Final value: {}", val);
    if val == FINAL_VALUE {
        println!("Antagonistic schedule reached, failing.");
        std::process::exit(1);
    }
    println!("Did not find antagonistic schedule. Succeeding.");
}
