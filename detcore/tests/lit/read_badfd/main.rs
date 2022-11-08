// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %me

// FIXME(T103294612): %hermit run --strict --verify -- %me

use nix::errno::Errno;
use nix::unistd::read;

fn main() {
    let mut buf = [0u8; 4];
    assert_eq!(read(9999, &mut buf), Err(Errno::EBADF));
}
