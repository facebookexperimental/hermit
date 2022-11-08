// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %me

use nix::fcntl::openat;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

fn main() {
    let (fd3, fd4) = nix::unistd::pipe().unwrap();
    assert_eq!(fd4, fd3 + 1);

    let _ = nix::unistd::close(fd3);

    assert_eq!(
        openat(libc::AT_FDCWD, "/dev/null", OFlag::O_RDONLY, Mode::S_IRUSR),
        Ok(fd3)
    );
}
