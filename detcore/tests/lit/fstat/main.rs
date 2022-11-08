/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Checks that we can observe mod time changes.

// RUN: %me

// FIXME: 'hermit run --strict --verify' does not work! This is likely caused by
// the non-determinism bug with st_mtime (see `hermit-run-strict.lit`).

use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::thread::sleep;
use std::time::Duration;

use nix::sys::stat::fstat;
use tempfile::NamedTempFile;

fn main() {
    let mut file = NamedTempFile::new().unwrap();

    let fd = file.as_raw_fd();

    let stat1 = fstat(fd).unwrap();

    println!("stat1: {:#?}", stat1);

    // Make sure nothing changes with subsequent fstat calls.
    for _ in 0..5 {
        assert_eq!(stat1, fstat(fd).unwrap());
    }

    // Sleep to ensure enough real time elapses such that the mod time changes
    // with the write.
    sleep(Duration::from_millis(10));

    // Update the file. There's no buffering, so this should translate to a
    // syscall.
    file.write_all(b"hello\n").unwrap();

    let stat2 = fstat(fd).unwrap();

    println!("stat2: {:#?}", stat2);

    assert!(
        stat2.st_mtime > stat1.st_mtime || stat2.st_mtime_nsec > stat1.st_mtime_nsec,
        "Expected mtime to advance"
    );
}
