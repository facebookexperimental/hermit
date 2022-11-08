// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %me

use std::ffi::CString;
use std::io::Write;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;

use nix::sys::stat;
use tempfile::NamedTempFile;

fn main() {
    let mut start: MaybeUninit<libc::timespec> = MaybeUninit::uninit();
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, start.as_mut_ptr()) },
        0
    );
    let start: libc::timespec = unsafe { start.assume_init() };
    let mut file = NamedTempFile::new().unwrap();
    assert!(writeln!(file, "hello, world!").is_ok());

    let path = CString::new(file.path().as_os_str().as_bytes()).unwrap();

    assert_eq! {
        unsafe {
            libc::utimes(path.as_ptr(), std::ptr::null_mut())
        },
        0
    }

    let statbuf = stat::stat(file.path()).unwrap();

    assert!(
        statbuf.st_atime >= start.tv_sec,
        "{} should be >= {}",
        statbuf.st_atime,
        start.tv_sec
    );
    assert!(
        statbuf.st_mtime >= start.tv_sec,
        "{} should be >= {}",
        statbuf.st_mtime,
        start.tv_sec
    );

    let next = libc::timeval {
        tv_sec: 1 + start.tv_sec,
        tv_usec: start.tv_nsec / 1000,
    };

    let tvs = [next, next];

    assert_eq! {
        unsafe {
            libc::utimes(path.as_ptr(), &tvs as *const _)
        },
        0
    };
}
