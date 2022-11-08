/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me

use nix::fcntl::openat;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

fn main() {
    // Make sure stdin is closed. This should get reused by openat. Ignore the
    // error in case it wasn't inherited from the parent process.
    let _ = nix::unistd::close(0);

    assert_eq!(
        openat(libc::AT_FDCWD, "/dev/null", OFlag::O_RDONLY, Mode::S_IRUSR),
        Ok(0)
    );
}
