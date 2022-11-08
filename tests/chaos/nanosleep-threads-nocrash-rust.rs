/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/// Let two child threads race, and arbitrarily define one order as correct and the other as error.
/// This is roughly the same as the C version by the same name, but Rust generates different code
/// provides us another test and in particular another test of stacktrace printing.
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;

fn sleep(milli: i64) {
    let tp = libc::timespec {
        tv_sec: milli / 1_000,
        tv_nsec: milli % 1_000 * 1_000_000,
    };
    unsafe {
        // issue a raw nanosleep syscall to the OS
        libc::nanosleep(&tp, std::ptr::null_mut())
    };
}

#[inline(never)]
fn mythread1_fun(var: Arc<AtomicUsize>) {
    sleep(1000);
    var.store(1, SeqCst);
}

#[inline(never)]
fn mythread2_fun(var: Arc<AtomicUsize>) {
    sleep(1000);
    var.store(2, SeqCst);
}

fn main() {
    let d = Arc::new(AtomicUsize::new(0));
    let d1 = Arc::clone(&d);
    let d2 = Arc::clone(&d);

    let h1 = std::thread::Builder::new()
        .name("mythread1".to_string())
        .spawn(move || mythread1_fun(d1))
        .unwrap();

    let h2 = std::thread::Builder::new()
        .name("mythread2".to_string())
        .spawn(move || mythread2_fun(d2))
        .unwrap();

    // mythread1(d1);
    h1.join().unwrap();
    h2.join().unwrap();
    let final_val = d.load(SeqCst);
    println!("Final value: {}", final_val);
    if final_val == 2 {
        std::process::exit(0);
    } else {
        std::process::exit(1);
    }
}
