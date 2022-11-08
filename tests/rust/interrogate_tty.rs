/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*!

 Run with rust-script if you like:

 ```cargo
 [dependencies]
 libc = "0.2.94"
 ```

*/

use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;

use nix::sys::stat::fstat;

fn main() {
    println!("Hello stdout.");
    eprintln!("Hello stderr.");

    // TODO: all the below println!s were previously writeln!s to this stdout handle (fd 4).
    // This currently causes a bug with record/replay where the output is not produced on replay.
    // rr is able to replay the output, and dealias the fds to realize it's actually stdout.
    // let mut stdout = fs::OpenOptions::new()
    //     .read(true)
    //     .write(true)
    //     .open("/proc/self/fd/1")
    //     .unwrap();

    unsafe {
        // writeln!(stdout, "stdin isatty = {:?}", libc::isatty(0)).unwrap();
        println!("stdin isatty = {:?}", libc::isatty(0));
        println!("stdout isatty = {:?}", libc::isatty(1));
        println!("stderr isatty = {:?}", libc::isatty(2));
    }
    println!("fstat(0) = {:?}", fstat(0));
    println!("fstat(1) = {:?}", fstat(1));
    println!("fstat(2) = {:?}", fstat(2));

    if let Ok(mut tty) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        let tty_fd = tty.as_raw_fd();
        writeln!(tty, "Hello TTY").unwrap();
        unsafe {
            println!("/dev/tty isatty = {:?}", libc::isatty(tty_fd));
        }
        println!("fstat(/dev/tty) = {:?}", fstat(tty_fd));
    } else {
        eprintln!("WARNING: This machine does not have /dev/tty available. Skipping part of test.");
    }
}
