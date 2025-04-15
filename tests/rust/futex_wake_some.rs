/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

/// Multiple threads wait on a futex, some are woken but not all.
use libc::c_long;
use nix::errno::Errno;

const TOTAL: i64 = 4;
const TO_WAKE: i64 = 2;

fn main() {
    let layout = std::alloc::Layout::from_size_align(4, 4).unwrap(); // u32
    let ptr: *mut u8 = unsafe { std::alloc::alloc_zeroed(layout) };
    let ptr_val = ptr as usize;

    let mut handles = Vec::new();
    let children_pre = Arc::new(AtomicI64::new(0));
    let children_post = Arc::new(AtomicI64::new(0));
    for ix in 0..TOTAL {
        let children_pre = children_pre.clone();
        let children_post = children_post.clone();
        let hndl = std::thread::spawn(move || {
            let ptr = ptr_val as *mut u8;
            children_pre.fetch_add(1, Ordering::SeqCst);
            let val: c_long = unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    ptr,
                    libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                    0, // val,
                    0, // timeout,
                    0, // uaddr - ignored
                    0, // val3 - ignored
                )
            };
            if val == 0 {
                children_post.fetch_add(1, Ordering::SeqCst);
                eprintln!("tid {} woken", ix);
            } else {
                println!(
                    "UNLIKELY: child thread {} issued wait after wake already happened, errno {}",
                    ix,
                    Errno::from_raw(Errno::last_raw())
                );
            }
        });
        handles.push(hndl);
    }

    // Without messing around in /proc we're not going to know for sure the waiters are actually
    // waiting.  But here's a hack that works for hermit stricts default behavior:
    while children_pre.load(Ordering::SeqCst) < TOTAL {}
    std::thread::sleep(std::time::Duration::from_millis(300));
    unsafe {
        *ptr = 1; // Mutate value, linearization point which will prevent futex waits.
    }
    let woke: i64 = unsafe {
        // At an application level, this is a bogus wake because the value hasn't actually changed.
        libc::syscall(
            libc::SYS_futex,
            ptr,
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            TO_WAKE, // val: wake specific number of threads
            0,       // timeout - ignored
            0,       // uaddr - ignored
            0,       // val3 - ignored
        )
    };
    assert_eq!(woke, TO_WAKE);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let checked_in = children_post.load(Ordering::SeqCst);
    assert!(checked_in <= TO_WAKE);
    let _ = unsafe { libc::syscall(libc::SYS_exit_group, 0) };
}
