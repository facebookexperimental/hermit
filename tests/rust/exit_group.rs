/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The simplest possible exit_group test.

fn main() {
    let _ = std::thread::spawn(move || {
        loop {
            print!("")
        }
    });
    let _ = unsafe { libc::syscall(libc::SYS_exit_group, 0) };
}
