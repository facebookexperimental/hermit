/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression guest for shutdown(2) (KVM ratchet round 12).
//!
//! shutdown is the lone remaining member of the socket family that was
//! classified Unsupported (fail-closing under --strict) while all of its
//! siblings were Determinized. This guest exercises SHUT_WR followed by a read
//! that must observe EOF, proving Detcore forwards shutdown and its half-close
//! effect is visible to the peer. Runs deterministically under
//! `hermit run --strict --verify`.

use std::os::fd::AsRawFd;

use nix::sys::socket::AddressFamily;
use nix::sys::socket::SockFlag;
use nix::sys::socket::SockType;
use nix::sys::socket::socketpair;

fn main() {
    if matches!(std::env::var("HERMIT_MODE"), Ok(mode) if mode == "record") {
        // Record mode currently hangs for socketpair-based guests; mirror the
        // sibling socketpair test's skip.
        eprintln!("Skipping test in record mode.");
        return;
    }

    let (sock1, sock2) = socketpair(
        AddressFamily::Unix,
        SockType::Stream,
        None,
        SockFlag::empty(),
    )
    .unwrap();

    // Normal write/read across the pair works before shutdown.
    nix::unistd::write(&sock1, b"Hello").unwrap();
    let mut buf = [0u8; 5];
    let n = nix::unistd::read(&sock2, &mut buf).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf[..], b"Hello");

    // Half-close the write direction of sock1. This is the syscall under test
    // (glibc's shutdown wrapper issues SYS_shutdown directly).
    let rc = unsafe { libc::shutdown(sock1.as_raw_fd(), libc::SHUT_WR) };
    assert_eq!(rc, 0, "shutdown(SHUT_WR) should succeed, got errno");

    // After SHUT_WR on sock1, the peer sock2 must observe EOF (0 bytes).
    let mut buf2 = [0u8; 8];
    let eof = nix::unistd::read(&sock2, &mut buf2).unwrap();
    assert_eq!(eof, 0, "peer read after SHUT_WR must return EOF");

    // A full shutdown of the remaining socket is also accepted deterministically.
    let rc2 = unsafe { libc::shutdown(sock2.as_raw_fd(), libc::SHUT_RDWR) };
    assert_eq!(rc2, 0, "shutdown(SHUT_RDWR) should succeed, got errno");

    println!("shutdown(2) half-close observed EOF. Test complete.");
}
