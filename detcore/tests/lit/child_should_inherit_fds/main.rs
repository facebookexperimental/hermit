/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me

// FIXME(T96027871): %hermit run --strict -- %me

use std::os::fd::AsRawFd;

use close_err::Closable;
use nix::sys::wait::WaitStatus;
use nix::sys::wait::waitpid;
use nix::unistd::ForkResult;
use nix::unistd::fork;
use nix::unistd::pipe;
use nix::unistd::read;
use nix::unistd::write;

fn main() {
    let (fdread, fdwrite) = pipe().unwrap();
    let msg: [u8; 8] = [0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x8];
    match unsafe { fork().unwrap() } {
        ForkResult::Parent { child, .. } => {
            fdwrite.close().expect("close failed");
            let mut buf: [u8; 8] = [0; 8];
            // XXX: The following SYS_read would timeout due to detcore issue.
            // add a 5 seconds timeout to abort.
            unsafe { libc::alarm(5) };
            assert_eq!(read(fdread.as_raw_fd(), &mut buf), Ok(8));
            assert_eq!(buf, msg);
            assert_eq!(waitpid(child, None), Ok(WaitStatus::Exited(child, 0)));
            unsafe { libc::syscall(libc::SYS_exit_group, 0) };
        }
        ForkResult::Child => {
            fdread.close().expect("close failed");
            assert_eq!(write(&fdwrite, &msg), Ok(8));
            unsafe { libc::syscall(libc::SYS_exit_group, 0) };
        }
    }
}
