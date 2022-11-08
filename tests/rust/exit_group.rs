// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! The simplest possible exit_group test.

fn main() {
    let _ = std::thread::spawn(move || {
        loop {
            print!("")
        }
    });
    let _ = unsafe { libc::syscall(libc::SYS_exit_group, 0) };
}
