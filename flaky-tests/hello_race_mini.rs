/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! One of the simplest possible race conditions.
//! Exits with a nonzero code under one order, and not the other.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;

#[inline(never)]
fn thread1(var: Arc<AtomicUsize>) {
    var.store(1, SeqCst);
}

fn main() {
    let d = Arc::new(AtomicUsize::new(1));
    let d1 = Arc::clone(&d);

    let h1 = std::thread::spawn(move || thread1(d1));
    d.store(2, SeqCst);
    h1.join().unwrap();

    let val = Arc::try_unwrap(d).unwrap().into_inner();
    println!("Final value: {}", val);
    if val == 2 {
        println!("Antagonistic schedule reached, failing.");
        std::process::exit(1);
    }
    println!("Did not find antagonistic schedule. Succeeding.");
}
