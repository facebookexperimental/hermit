// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %me | FileCheck %s
// CHECK: {{([ab]+)}}
// CHECK-EMPTY:

fn main() {
    let child = std::thread::spawn(move || {
        for _ in 0..200 {
            nix::unistd::write(1, b"a").unwrap();
        }
    });
    for _ in 0..200 {
        nix::unistd::write(1, b"b").unwrap();
    }
    child.join().unwrap();
    nix::unistd::write(1, b"\n").unwrap();
}
