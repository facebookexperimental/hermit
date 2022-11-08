// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

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
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time;

use nix::poll::poll;
use nix::poll::PollFd;
use nix::poll::PollFlags;
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
            let poll1 = PollFd::new(fdread, PollFlags::POLLIN);
            let mut slice = [poll1];
            let res = poll(&mut slice, 0); // 0 timeout
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
        assert!(unistd::close(fdread).is_ok());
    });

    while count2.load(Ordering::SeqCst) == 0 {
        thread::sleep(time::Duration::from_nanos(5000));
    }

    println!("Parent writing to pipe..");
    let msg = "hello world\n";
    assert_eq!(unistd::write(fdwrite, msg.as_bytes()), Ok(12));
    assert!(unistd::close(fdwrite).is_ok());

    println!("Joining child..");
    handle.join().unwrap();
}
