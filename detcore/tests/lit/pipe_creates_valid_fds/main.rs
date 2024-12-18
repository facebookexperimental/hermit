/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me
use std::os::fd::AsRawFd;

use close_err::Closable;
use nix::sys::stat::fstat;
use nix::unistd::pipe;

fn main() {
    let (fd_read, fd_write) = pipe().unwrap();
    assert!(fstat(fd_read.as_raw_fd()).is_ok());
    assert!(fstat(fd_write.as_raw_fd()).is_ok());
    fd_write.close().expect("close failed");
    fd_read.close().expect("close failed");
}
