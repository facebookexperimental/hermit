/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Simple program to print out a pointer to the stack. This helps to test that
//! our address space is deterministic and that the stack is consistent across
//! runs.

fn main() {
    let x = 42;
    println!("Stack pointer: {:p}", &x as *const _);
}
