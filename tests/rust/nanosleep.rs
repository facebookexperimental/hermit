// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

/*! Make sure sleeping all threads doesn't get us stuck.

 Run with rust-script if you like:

 ```cargo
 [dependencies]
 libc = "0.2.94"

 [build]
 target = "x86_64-unknown-linux-musl"

 ```

*/

fn sleep(milli: i64) {
    let tp = libc::timespec {
        tv_sec: milli / 1_000,
        tv_nsec: milli % 1_000 * 1_000_000,
    };
    unsafe {
        // issue a raw nanosleep syscall to the OS
        libc::nanosleep(&tp, std::ptr::null_mut())
    };
}

fn main() {
    // 170ms is just a random amount of time that makes the test fast to complete.
    sleep(170);
}
