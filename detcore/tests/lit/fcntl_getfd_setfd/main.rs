/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me

use nix::fcntl::fcntl;
use nix::fcntl::FcntlArg;
use nix::fcntl::FdFlag;
use syscalls::syscall;
use syscalls::Sysno;

fn main() {
    // Raw syscall used here because unistd::dup3 is simulated from dup2
    let dup_fd = unsafe { syscall!(Sysno::dup3, 2, 255, libc::O_CLOEXEC) }.unwrap() as i32;
    assert_eq!(dup_fd, 255);

    let flag = fcntl(dup_fd, FcntlArg::F_GETFD);
    assert_eq!(flag, Ok(FdFlag::FD_CLOEXEC.bits()));
}
