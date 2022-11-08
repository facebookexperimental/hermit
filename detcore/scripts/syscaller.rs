// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        panic!("Please provide an integer argument");
    }

    let syscall_num: i64 = args[1].parse().unwrap();

    unsafe {
        libc::syscall(syscall_num, 0, 0, 0, 0, 0, 0);
    }
}
