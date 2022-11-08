// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! Wait on a futex with a timeout.
use nix::errno::errno;
use nix::errno::Errno;

fn main() {
    let layout = std::alloc::Layout::from_size_align(4, 4).unwrap(); // u32
    let ptr: *mut u8 = unsafe { std::alloc::alloc_zeroed(layout) };

    eprintln!("main start futex wait ({:?})..", ptr);
    let tp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 10_000_000,
    };
    let res = unsafe {
        libc::syscall(
            libc::SYS_futex,
            ptr,
            libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
            0,   // val,
            &tp, // timeout,
            0,   // uaddr - ignored
            0,   // val3 - ignored
        )
    };
    let err = errno();
    eprintln!(
        "Main thread: futex wait timed out and returned {}, errno {:?}",
        res,
        Errno::from_i32(err)
    );
}
