/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression guest for determinized `recvmmsg(2)` (the multi-message sibling
//! of `recvmsg`). Before it was reclassified Determinized in Detcore, running
//! this under `--strict` aborted with an unsupported-syscall error. It receives
//! two datagrams queued on a `SOCK_DGRAM` socketpair in a single `recvmmsg`
//! call and checks both message bodies and lengths.

use std::io::IoSliceMut;
use std::os::fd::AsRawFd;

use nix::sys::socket::AddressFamily;
use nix::sys::socket::MsgFlags;
use nix::sys::socket::MultiHeaders;
use nix::sys::socket::SockFlag;
use nix::sys::socket::SockType;
use nix::sys::socket::recvmmsg;
use nix::sys::socket::socketpair;

fn main() {
    if matches!(std::env::var("HERMIT_MODE"), Ok(mode) if mode == "record") {
        // Record mode currently hangs for socketpair-based tests; skip to match
        // the sibling socketpair guest.
        eprintln!("Skipping test in record mode.");
        return;
    }

    let (sender, receiver) = socketpair(
        AddressFamily::Unix,
        SockType::Datagram,
        None,
        SockFlag::empty(),
    )
    .unwrap();

    let payloads: [&[u8]; 2] = [b"hello", b"world!"];
    for payload in payloads.iter() {
        nix::unistd::write(&sender, payload).unwrap();
    }

    // Two receive slots, each with a 32-byte buffer. The recvmmsg call writes
    // into `buffers` through `iovs`; we capture the per-message byte counts and
    // then release the mutable borrow of `buffers` before inspecting its bytes.
    let mut buffers = [[0u8; 32], [0u8; 32]];
    let lengths: Vec<usize> = {
        let mut iovs: Vec<[IoSliceMut; 1]> = buffers
            .iter_mut()
            .map(|b| [IoSliceMut::new(&mut b[..])])
            .collect();
        let mut headers = MultiHeaders::<()>::preallocate(iovs.len(), None);

        recvmmsg(
            receiver.as_raw_fd(),
            &mut headers,
            iovs.iter_mut(),
            MsgFlags::MSG_DONTWAIT,
            None,
        )
        .unwrap()
        .map(|msg| msg.bytes)
        .collect()
    };

    assert_eq!(lengths.len(), 2, "expected two datagrams in one recvmmsg");
    for (i, (len, expected)) in lengths.iter().zip(payloads.iter()).enumerate() {
        assert_eq!(*len, expected.len(), "message {i} had unexpected length");
        assert_eq!(&buffers[i][..expected.len()], *expected, "message {i} body");
    }

    println!("recvmmsg received 2 messages. Test complete.");
}
