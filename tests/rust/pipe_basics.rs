/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::fd::AsRawFd;
use std::str;

use nix::unistd;

fn main() {
    let (fdread, fdwrite) = unistd::pipe().unwrap();
    let handle = std::thread::spawn(move || {
        let mut buf: [u8; 14] = [0; 14];
        for _ in 10..20 {
            assert_eq!(unistd::read(fdread.as_raw_fd(), &mut buf), Ok(14));
            println!("Child received message: {}", str::from_utf8(&buf).unwrap());
        }
        drop(fdread);
    });

    for ix in 10..20 {
        println!("Parent writing to pipe..");
        let msg = format!("hello world {}", ix);
        assert_eq!(unistd::write(&fdwrite, msg.as_bytes()), Ok(14));
    }
    drop(fdwrite);

    println!("Joining child..");
    handle.join().unwrap();
}
