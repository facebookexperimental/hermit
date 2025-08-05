/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me
use std::os::fd::AsRawFd;
use std::os::fd::IntoRawFd;

use close_err::Closable;
use nix::fcntl::AT_FDCWD;
use nix::fcntl::OFlag;
use nix::fcntl::openat;
use nix::sys::stat::Mode;

fn main() {
    let (fd3, fd4) = nix::unistd::pipe().unwrap();
    assert_eq!(fd4.as_raw_fd(), fd3.as_raw_fd() + 1);

    let fd3_raw = fd3.as_raw_fd();
    fd3.close().expect("close failed");

    assert_eq!(
        openat(AT_FDCWD, "/dev/null", OFlag::O_RDONLY, Mode::S_IRUSR).map(|f| f.into_raw_fd()),
        Ok(fd3_raw)
    );
}
