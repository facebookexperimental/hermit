/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*!

Spin poll() repeatedly until it succeeds.  Intentionally nondeterministic to stress determinization.

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
use std::os::fd::AsFd;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time;

use close_err::Closable;
use nix::poll::poll;
use nix::poll::PollFd;
use nix::poll::PollFlags;
use nix::poll::PollTimeout;
use nix::unistd;

fn main() {
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();
    let (fdread, fdwrite) = unistd::pipe().unwrap();
    let handle = std::thread::spawn(move || {
        let mut work = 1;
        loop {
            work = u64::max(2 * work, 10 ^ 9);
            // let poll1 = PollFd::new(fdread.as_raw_fd(), PollFlags::POLLIN);
            let poll1 = PollFd::new(fdread.as_fd(), PollFlags::POLLIN);
            let mut slice = [poll1];
            let res = poll(&mut slice, PollTimeout::ZERO); // 0 timeout
            println!(
                "Poll result: {:?}, (count {})",
                res,
                count.load(Ordering::SeqCst)
            );
            if res == Ok(0) || res.is_err() {
                for _ in 0..work {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                continue;
            } else {
                println!("Final fdread poll result: {:?}", slice);
                break;
            }
        }
        fdread.close().expect("close failed");
    });

    while count2.load(Ordering::SeqCst) == 0 {
        thread::sleep(time::Duration::from_nanos(5000));
    }

    println!("Parent writing to pipe..");
    let msg = "hello world\n";
    assert_eq!(unistd::write(&fdwrite, msg.as_bytes()), Ok(12));
    fdwrite.close().expect("close failed");

    println!("Joining child..");
    handle.join().unwrap();
}
