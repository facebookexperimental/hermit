/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*! Exercise some options of SYS_poll.

 Run with rust-script if you like:

 ```cargo
 [dependencies]
 nix = "0.20.0"
 tempfile = "3.2.0"

 [build]
 target = "x86_64-unknown-linux-musl"

 [target.x86_64-unknown-linux-musl]
 ```

*/
use std::fs;
use std::io::Write;
use std::os::fd::AsFd;

use nix::poll::poll;
use nix::poll::PollFd;
use nix::poll::PollFlags;
use nix::poll::PollTimeout;
use tempfile::tempdir;

fn main() {
    println!("Empty poll to sleep 300ms:");
    poll(&mut [], 300u16).expect("poll failed");
    println!("Finished empty poll.");

    {
        let dir = tempdir().unwrap();
        let path1 = dir.path().join("file1.txt");
        let path2 = dir.path().join("file2.txt");
        {
            let mut file1 = fs::File::create(&path1).unwrap();
            let mut file2 = fs::File::create(&path2).unwrap();
            file1.write_all(b"text1").unwrap();
            file2.write_all(b"text2").unwrap();
        }

        for timeout in &[PollTimeout::NONE, PollTimeout::ZERO, 300u16.into()] {
            let file1 = fs::File::open(&path1).unwrap();
            let file2 = fs::File::open(&path2).unwrap();
            let poll1 = PollFd::new(file1.as_fd(), PollFlags::all());
            let poll2 = PollFd::new(file2.as_fd(), PollFlags::empty());
            let mut slice = [poll1, poll2];

            println!(
                "\nPoll on two created temp files, fds {:?}, timeout={:?}:",
                slice, timeout,
            );
            let events = poll(&mut slice, *timeout).expect("poll failed");
            println!("Done poll.");
            // We should only hear back from ONE of these, because PollFlags::empty should
            // mean no events on the second.
            assert_eq!(events, 1);

            let flags1 = slice[0].revents().unwrap();
            let flags2 = slice[1].revents().unwrap();
            println!("Poll results on file1: {:?}", flags1);
            println!("Poll results on file2: {:?}", flags2);
            assert!(!flags1.is_empty());
            assert!(flags2.is_empty());
        }
    }
}
