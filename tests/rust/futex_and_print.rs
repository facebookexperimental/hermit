/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::sync::Arc;

const TOTAL: i64 = 4;

#[inline(never)]
fn noopy(counter: &Arc<AtomicI64>) {
    counter.fetch_add(1, Ordering::SeqCst);
    counter.fetch_add(-1, Ordering::SeqCst);
}

fn main() {
    let counter = Arc::new(AtomicI64::new(0));
    for _ix in 0..TOTAL {
        let counter = counter.clone();
        let _ = std::thread::spawn(move || {
            println!("Child thread starting..");
            counter.fetch_add(1, Ordering::SeqCst);
        });
    }
    while counter.load(Ordering::SeqCst) < TOTAL {
        noopy(&counter);
    }
    println!("Done.");
    let _ = unsafe { libc::syscall(libc::SYS_exit_group, 0) };
}
