/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me | FileCheck %s
// CHECK: {{([ab]+)}}
// CHECK-EMPTY:
use std::os::fd::BorrowedFd;

fn main() {
    let child = std::thread::spawn(move || {
        for _ in 0..200 {
            nix::unistd::write(unsafe { BorrowedFd::borrow_raw(1) }, b"a").unwrap();
        }
    });
    for _ in 0..200 {
        nix::unistd::write(unsafe { BorrowedFd::borrow_raw(1) }, b"b").unwrap();
    }
    child.join().unwrap();
    nix::unistd::write(unsafe { BorrowedFd::borrow_raw(1) }, b"\n").unwrap();
}
