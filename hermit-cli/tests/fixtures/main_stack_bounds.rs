/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The main thread's stack bounds as a Rust program reads them. Rust's
//! standard library takes them at startup from glibc's `pthread_getattr_np`,
//! which finds the main thread's stack in `/proc/self/maps`, and puts the
//! guard its stack overflow handler reports on the page below the low end
//! (`install_main_guard_linux` in std's `stack_overflow.rs`).
//!
//! The program prints the line of `/proc/self/maps`, read through `std::fs`,
//! that holds a local of `main`, and what `pthread_getattr_np` answers for
//! the main thread: `stack TOP SIZE`. With the argument `overflow`, it then
//! runs itself in a child that recurses on its main thread until the stack
//! overflows, and prints how the child ended and whether std reported the
//! overflow, which it does only for a fault in the guard it computed.
//!
//! Usage: main_stack_bounds [overflow]

use std::ffi::c_void;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

/// glibc's `pthread_attr_t` on x86-64: 56 bytes, aligned as a pointer.
#[repr(C)]
struct PthreadAttr([u64; 7]);

unsafe extern "C" {
    fn pthread_self() -> usize;
    fn pthread_getattr_np(thread: usize, attr: *mut PthreadAttr) -> i32;
    fn pthread_attr_getstack(
        attr: *const PthreadAttr,
        stackaddr: *mut *mut c_void,
        stacksize: *mut usize,
    ) -> i32;
    fn pthread_attr_destroy(attr: *mut PthreadAttr) -> i32;
}

/// Recurses until the main thread's stack overflows: each frame keeps 4 KiB
/// live, so every page of the stack is written on the way down.
#[inline(never)]
fn recurse(depth: u64) -> u64 {
    let frame = std::hint::black_box([depth; 512]);
    if depth == u64::MAX {
        return frame[0];
    }
    recurse(depth + 1).wrapping_add(frame[511])
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("recurse") {
        println!("{}", recurse(0));
        return;
    }
    let local = 0_u8;
    let here = std::hint::black_box(&local) as *const u8 as usize;
    let maps = std::fs::read_to_string("/proc/self/maps").expect("read /proc/self/maps");
    for line in maps.lines() {
        let range = line.split(' ').next().expect("a maps line has a range");
        let (from, to) = range.split_once('-').expect("a range has a dash");
        let from = usize::from_str_radix(from, 16).expect("a hexadecimal start");
        let to = usize::from_str_radix(to, 16).expect("a hexadecimal end");
        if from <= here && here < to {
            println!("maps {line}");
        }
    }
    let mut attr = PthreadAttr([0; 7]);
    let mut low: *mut c_void = std::ptr::null_mut();
    let mut size = 0_usize;
    // SAFETY: `attr` is a pthread_attr_t sized buffer that pthread_getattr_np
    // initializes and pthread_attr_destroy releases; the outputs are locals.
    unsafe {
        assert_eq!(pthread_getattr_np(pthread_self(), &mut attr), 0);
        assert_eq!(pthread_attr_getstack(&attr, &mut low, &mut size), 0);
        assert_eq!(pthread_attr_destroy(&mut attr), 0);
    }
    println!("stack {:x} {:x}", low as usize + size, size);
    if args.get(1).map(String::as_str) == Some("overflow") {
        let child = Command::new(&args[0])
            .arg("recurse")
            .output()
            .expect("run the recursing child");
        let stderr = String::from_utf8_lossy(&child.stderr);
        println!(
            "overflow signal {:?} reported {}",
            child.status.signal(),
            // `thread 'main' (TID) has overflowed its stack`, whatever the
            // thread's number.
            stderr.starts_with("\nthread 'main' ")
                && stderr.contains(" has overflowed its stack\n"),
        );
    }
}
