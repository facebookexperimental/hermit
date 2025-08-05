/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me

// FIXME(T103294612): %hermit run --strict --verify -- %me

use std::os::fd::BorrowedFd;

use nix::errno::Errno;
use nix::unistd::read;

fn main() {
    let mut buf = [0u8; 4];
    assert_eq!(
        read(unsafe { BorrowedFd::borrow_raw(9999) }, &mut buf),
        Err(Errno::EBADF)
    );
}
