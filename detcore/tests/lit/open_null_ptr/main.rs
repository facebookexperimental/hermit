// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %me

fn main() {
    assert_eq!(unsafe { libc::open(core::ptr::null(), 0, 0) }, -1);
}
