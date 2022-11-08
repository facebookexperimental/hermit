// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! Simple program to print out a pointer to the stack. This helps to test that
//! our address space is deterministic and that the stack is consistent across
//! runs.

fn main() {
    let x = 42;
    println!("Stack pointer: {:p}", &x as *const _);
}
