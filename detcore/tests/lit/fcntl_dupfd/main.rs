/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me

use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;

use nix::fcntl::FcntlArg;
use nix::fcntl::fcntl;
use nix::sys::stat::fstat;
use nix::unistd::close;
use nix::unistd::dup2_raw;

fn main() {
    let stdin_duped = unsafe { dup2_raw(std::io::stdin(), 100) }.unwrap();
    assert_eq!(stdin_duped.as_raw_fd(), 100);

    let dup_fd = fcntl(std::io::stdin(), FcntlArg::F_DUPFD_CLOEXEC(100)).unwrap();
    assert_eq!(dup_fd, 101);
    let dup_fd = unsafe { OwnedFd::from_raw_fd(dup_fd) };

    assert!(fstat(&dup_fd).is_ok());
    assert!(close(dup_fd).is_ok());
}
