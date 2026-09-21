/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression guest for determinized `select(2)` (the `timeval` sibling of
//! `pselect6`). Before it was reclassified Determinized in Detcore, running this
//! under `--strict` aborted with an unsupported-syscall error. It exercises both
//! the ready path (a pipe with pending data reports its read end readable) and
//! the timeout path (an empty pipe times out and `select` returns 0). A pipe is
//! used deliberately so the guest does not depend on socket support.
//!
//! NOTE: on x86-64 glibc's `select()` wrapper is implemented on top of the
//! `pselect6` syscall, so this guest issues the raw `select` syscall directly to
//! actually cover Detcore's `handle_select` path rather than `handle_pselect6`.

use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;

use nix::unistd::pipe;
use nix::unistd::write;

/// Invoke the raw `select(2)` syscall (bypassing glibc's pselect6-based wrapper)
/// and return its raw result plus the (possibly updated) timeout that Linux
/// writes back with the time not slept.
fn raw_select(read_fd: i32, timeout: &mut libc::timeval) -> i64 {
    // SAFETY: fd_set is a plain bitmap; we zero it and set a single valid fd.
    let mut readfds: libc::fd_set = unsafe { std::mem::zeroed() };
    unsafe { libc::FD_ZERO(&mut readfds) };
    unsafe { libc::FD_SET(read_fd, &mut readfds) };

    // SAFETY: readfds and timeout are valid; writefds/exceptfds are NULL.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_select,
            read_fd + 1,
            &mut readfds as *mut libc::fd_set,
            std::ptr::null_mut::<libc::fd_set>(),
            std::ptr::null_mut::<libc::fd_set>(),
            timeout as *mut libc::timeval,
        )
    };
    if ret > 0 {
        assert!(
            unsafe { libc::FD_ISSET(read_fd, &readfds) },
            "select reported a ready fd but the read end bit was clear"
        );
    }
    ret
}

fn main() {
    // Ready path: data queued in the pipe makes the read end selectable.
    let (ready_r, ready_w): (OwnedFd, OwnedFd) = pipe().unwrap();
    write(&ready_w, b"ping").unwrap();
    let mut timeout = libc::timeval {
        tv_sec: 5,
        tv_usec: 0,
    };
    let ret = raw_select(ready_r.as_raw_fd(), &mut timeout);
    assert_eq!(ret, 1, "read end with pending data should be readable");

    // Timeout path: an empty pipe blocks until the timeout elapses, then returns 0.
    let (empty_r, _empty_w): (OwnedFd, OwnedFd) = pipe().unwrap();
    let mut timeout = libc::timeval {
        tv_sec: 0,
        tv_usec: 10_000,
    };
    let ret = raw_select(empty_r.as_raw_fd(), &mut timeout);
    assert_eq!(ret, 0, "empty pipe select should time out with 0 ready fds");

    println!("select ready+timeout paths OK. Test complete.");
}
