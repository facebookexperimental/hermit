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
use nix::sys::wait::WaitStatus;
use nix::sys::wait::waitpid;
use nix::unistd::ForkResult;
use nix::unistd::alarm;
use nix::unistd::dup2;
use nix::unistd::fork;
use nix::unistd::pipe;
use nix::unistd::read;
use nix::unistd::write;

fn main() {
    let (fdread, fdwrite) = pipe().unwrap();

    let msg = [0x1u8, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x8];

    match unsafe { fork().unwrap() } {
        ForkResult::Parent { child, .. } => {
            fdwrite.close().expect("close failed");
            let mut buf = [0u8; 8];

            // If the file descriptor newfd was previously open,
            // it is silently closed before being reused.
            assert!(dup2(fdread.as_raw_fd(), libc::STDIN_FILENO).is_ok());

            // FIXME: The following SYS_read will timeout due to detcore
            // scheduler issue. Abort if we're still going in 5 seconds.
            alarm::set(5);

            assert_eq!(read(libc::STDIN_FILENO, &mut buf), Ok(msg.len()));
            assert_eq!(buf, msg);

            alarm::cancel();

            assert_eq!(waitpid(child, None), Ok(WaitStatus::Exited(child, 0)));
        }
        ForkResult::Child => {
            fdread.close().expect("close failed");
            assert_eq!(write(fdwrite, &msg), Ok(msg.len()));
        }
    }
}
