/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me

use nix::fcntl::FcntlArg;
use nix::fcntl::fcntl;
use nix::sys::stat::fstat;
use nix::unistd::close;
use nix::unistd::dup2;

fn main() {
    assert_eq!(dup2(0, 100), Ok(100));

    let dup_fd = fcntl(0, FcntlArg::F_DUPFD_CLOEXEC(100)).unwrap();
    assert_eq!(dup_fd, 101);

    assert!(fstat(dup_fd).is_ok());
    assert!(close(dup_fd).is_ok());
}
