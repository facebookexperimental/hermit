// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

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
