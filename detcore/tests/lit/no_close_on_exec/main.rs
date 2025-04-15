/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Tests that a file descriptor remains valid even after execve if O_CLOEXEC
//! wasn't specified. See also the `close_on_exec` test.

// RUN: %me

// FIXME(T96027871): %hermit run --strict -- %me

use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::ptr;

use close_err::Closable;
use nix::fcntl::OFlag;
use nix::sys::wait::WaitStatus;
use nix::sys::wait::waitpid;
use nix::unistd::ForkResult;
use nix::unistd::fork;
use nix::unistd::pipe2;
use nix::unistd::read;
use nix::unistd::write;

fn main() {
    let mut args = std::env::args();
    args.next();

    if let Some(fdwrite) = args.next() {
        // This fd is valid since it didn't have O_CLOEXEC specified.
        let fdwrite: i32 = fdwrite.parse().unwrap();
        let fdwrite = unsafe { BorrowedFd::borrow_raw(fdwrite) };

        assert_eq!(write(fdwrite, b"wassup"), Ok(6));

        return;
    }

    // We specifically DON'T pass O_CLOEXEC here. The write end remains valid in
    // the child process after execve.
    let (fdread, fdwrite) = pipe2(OFlag::empty()).unwrap();

    // Allocate the string before the fork to avoid deadlocks.
    let fdwrite_str = format!("{}\0", fdwrite.as_raw_fd());

    match unsafe { fork().unwrap() } {
        ForkResult::Parent { child, .. } => {
            fdwrite.close().expect("close failed");

            let mut msg = [0u8; 6];

            assert_eq!(read(fdread.as_raw_fd(), &mut msg), Ok(6));
            assert_eq!(&msg, b"wassup");

            // Wait for the child to exit.
            assert_eq!(waitpid(child, None), Ok(WaitStatus::Exited(child, 0)));
        }
        ForkResult::Child => {
            fdread.close().expect("close failed");

            let proc_self = "/proc/self/exe\0".as_ptr() as *const libc::c_char;

            // Execute thyself, passing in the pipe's file descriptor.
            unsafe {
                libc::execve(
                    proc_self,
                    &[proc_self, fdwrite_str.as_ptr() as *const _, ptr::null()] as *const *const _,
                    &[ptr::null()] as *const *const _,
                )
            };

            panic!("execve failed");
        }
    }
}
