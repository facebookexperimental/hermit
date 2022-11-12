/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use core::arch::x86_64::__rdtscp;
use core::arch::x86_64::_rdtsc;

#[inline(never)]
fn rdtsc() -> u64 {
    unsafe { _rdtsc() }
}

#[allow(unused_mut)]
#[inline(never)]
fn rdtscp() -> u64 {
    let mut _aux = 0;
    unsafe { __rdtscp(&mut _aux) }
}

fn main() {
    for _ in 0..16 {
        println!("rdtsc:  {:#x}", rdtsc());
        println!("rdtscp: {:#x}", rdtscp());
    }
}
