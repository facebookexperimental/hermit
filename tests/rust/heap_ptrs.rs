/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Test deterministic Rust heap allocation

#[allow(clippy::useless_vec)]
fn main() {
    // Go far enough that we trigger an mmap directly for a big allocation.
    // Interleaving print/alloc makes it easy to see which ones call mmap in an strace.
    let x1 = vec![0u8; 1];
    println!("alloc 1:          {:#p}", x1.as_ptr());
    let x2 = vec![0u8; 10];
    println!("alloc 10:         {:#p}", x2.as_ptr());
    let x3 = vec![0u8; 100];
    println!("alloc 100:        {:#p}", x3.as_ptr());
    let x4 = vec![0u8; 1000];
    println!("alloc 1000:       {:#p}", x4.as_ptr());
    let x5 = vec![0u8; 10000];
    println!("alloc 10,000:     {:#p}", x5.as_ptr());
    let x6 = vec![0u8; 100000];
    println!("alloc 100,000:    {:#p}", x6.as_ptr());
    let x7 = vec![0u8; 1000000];
    println!("alloc 1,000,000:  {:#p}", x7.as_ptr());
    let x8 = vec![0u8; 10000000];
    println!("alloc 10,000,000: {:#p}", x8.as_ptr());
}
