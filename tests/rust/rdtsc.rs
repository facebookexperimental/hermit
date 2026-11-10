/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::__rdtscp;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::_rdtsc;

#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn rdtsc() -> u64 {
    unsafe { _rdtsc() }
}

#[cfg(target_arch = "x86_64")]
#[allow(unused_mut)]
#[inline(never)]
fn rdtscp() -> u64 {
    let mut _aux = 0;
    unsafe { __rdtscp(&mut _aux) }
}

/// `rdtsc` and `rdtscp` are x86 instructions; elsewhere this guest only says
/// so, and fails.
#[cfg(not(target_arch = "x86_64"))]
fn main() {
    eprintln!("rdtsc and rdtscp are x86 instructions");
    std::process::exit(1);
}

#[cfg(target_arch = "x86_64")]
fn main() {
    for _ in 0..16 {
        println!("rdtsc:  {:#x}", rdtsc());
        println!("rdtscp: {:#x}", rdtscp());
    }
}
