// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %me

use nix::fcntl::OFlag;
use nix::sys::stat::fstat;
use nix::unistd::close;
use nix::unistd::dup;
use nix::unistd::dup2;
use nix::unistd::dup3;

fn main() {
    let dup_fd = dup2(0, 255).unwrap();
    assert_eq!(dup_fd, 255);
    assert!(fstat(dup_fd).is_ok());
    assert!(close(dup_fd).is_ok());

    // Don't care if this fails. We just need fd 3 to be free.
    let _ = close(3);

    let dup_fd = dup(2).unwrap();
    assert_eq!(dup_fd, 3);
    assert!(fstat(dup_fd).is_ok());
    assert!(close(dup_fd).is_ok());

    let dup_fd = dup3(2, 255, OFlag::empty()).unwrap();
    assert_eq!(dup_fd, 255);
    assert!(fstat(dup_fd).is_ok());
    assert!(close(dup_fd).is_ok());
}
