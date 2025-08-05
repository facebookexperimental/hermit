/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Tests that a file descriptor is automatically closed after execve if
//! O_CLOEXEC was specified. See also the `no_close_on_exec` test.

// RUN: %me

// FIXME(T96027871): %hermit run --strict -- %me

use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::ptr;

use close_err::Closable;
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::stat::fstat;
use nix::sys::wait::WaitStatus;
use nix::sys::wait::waitpid;
use nix::unistd::ForkResult;
use nix::unistd::fork;
use nix::unistd::pipe2;
use nix::unistd::read;

fn main() {
    let mut args = std::env::args();
    args.next();

    if let Some(fdwrite) = args.next() {
        // This fd shouldn't be valid since it was closed.
        let fdwrite: i32 = fdwrite.parse().unwrap();

        assert_eq!(
            // SAFETY: We're in a single-threaded process and know nothing else is re-using the fd
            fstat(unsafe { BorrowedFd::borrow_raw(fdwrite) }),
            Err(Errno::EBADF)
        );

        return;
    }

    let (fdread, fdwrite) = pipe2(OFlag::O_CLOEXEC).unwrap();

    // Allocate the string before the fork to avoid deadlocks.
    let fdwrite_str = format!("{}\0", fdwrite.as_raw_fd());

    match unsafe { fork().unwrap() } {
        ForkResult::Parent { child, .. } => {
            fdwrite.close().expect("close failed");

            let mut msg = [0u8; 4];

            // The child never writes to this fd before calling `execve`. This
            // is a common pattern when spawning processes for smuggling out the
            // `execve` errno.
            assert_eq!(read(&fdread, &mut msg), Ok(0));

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
