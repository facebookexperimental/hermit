/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me

use std::os::fd::AsRawFd;

use nix::fcntl::OFlag;
use nix::sys::stat::fstat;
use nix::unistd::close;
use nix::unistd::dup;
use nix::unistd::dup2_raw;
use nix::unistd::dup3_raw;

fn main() {
    let dup_fd = unsafe { dup2_raw(std::io::stdin(), 255) }.unwrap();
    assert_eq!(dup_fd.as_raw_fd(), 255);
    assert!(fstat(&dup_fd).is_ok());
    assert!(close(dup_fd).is_ok());

    // Don't care if this fails. We just need fd 3 to be free.
    let _ = close(3);

    let dup_fd = dup(std::io::stderr()).unwrap();
    assert_eq!(dup_fd.as_raw_fd(), 3);
    assert!(fstat(&dup_fd).is_ok());
    assert!(close(dup_fd).is_ok());

    let dup_fd = unsafe { dup3_raw(std::io::stderr(), 255, OFlag::empty()) }.unwrap();
    assert_eq!(dup_fd.as_raw_fd(), 255);
    assert!(fstat(&dup_fd).is_ok());
    assert!(close(dup_fd).is_ok());
}
