// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %me

use nix::sys::stat::fstat;
use nix::unistd::close;
use nix::unistd::pipe;

fn main() {
    let (fd_read, fd_write) = pipe().unwrap();
    assert!(fstat(fd_read).is_ok());
    assert!(fstat(fd_write).is_ok());
    assert!(close(fd_write).is_ok());
    assert!(close(fd_read).is_ok());
}
