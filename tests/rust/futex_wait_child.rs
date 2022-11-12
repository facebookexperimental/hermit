/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/// Child thread waits on futex.

fn main() {
    let layout = std::alloc::Layout::from_size_align(4, 4).unwrap(); // u32
    let ptr: *mut u8 = unsafe { std::alloc::alloc_zeroed(layout) };
    let ptr2 = ptr as usize;

    eprintln!("Parent thread: spawn child.");
    let _ = std::thread::spawn(move || {
        let ptr: *mut u8 = ptr2 as *mut u8;
        eprintln!("Child thread start.");
        eprintln!("Child thread: start futex wait ({:?})..", ptr);
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                ptr,
                libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                0, // val,
                0, // timeout,
                0, // uaddr - ignored
                0, // val3 - ignored
            );
        }
        eprintln!("Child thread: futex wait returned, done.");
    });
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Note: in a very unfair schedule this wake may go first and fizzle.  In that case, the child
    // thread will remain blocked when exit_group is called below.
    eprintln!("Parent thread: futex wake ({:?})..", ptr);
    let woke = unsafe {
        // At an application level, this is a bogus wake because the value hasn't actually changed.
        libc::syscall(
            libc::SYS_futex,
            ptr,
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            1, // val: wake 1 thread
            0, // timeout - ignored
            0, // uaddr - ignored
            0, // val3 - ignored
        )
    };
    std::thread::sleep(std::time::Duration::from_millis(500));
    eprintln!("Parent thread: done with futex wake: {}", woke);
    std::thread::sleep(std::time::Duration::from_millis(500));
    eprintln!("Parent thread: exiting process.");
    let _ = unsafe { libc::syscall(libc::SYS_exit_group, 0) };
}
