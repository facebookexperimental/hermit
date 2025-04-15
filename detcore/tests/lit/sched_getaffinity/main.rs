/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me

use nix::unistd::Pid;
use syscalls::Errno;
use syscalls::Sysno;
use syscalls::syscall;

fn raw_sched_getaffinity<const N: usize>() -> Result<(), Errno> {
    let mut buf = [0u8; N];

    unsafe { syscall!(Sysno::sched_getaffinity, 0, buf.len(), &mut buf as *mut u8) }?;

    // The resulting buffer should have at least 1 bit set.
    assert!(
        buf.iter().any(|byte| *byte != 0),
        "Affinity mask should not be all zeroes. Got {:?}",
        buf
    );

    Ok(())
}

fn main() {
    // Call via a wrapper, which allocates its own bitset.
    let _ = nix::sched::sched_getaffinity(Pid::from_raw(0)).unwrap();

    // Call with various buffer sizes to make sure we can handle them all.
    raw_sched_getaffinity::<128>().unwrap();
    raw_sched_getaffinity::<1024>().unwrap();
    raw_sched_getaffinity::<2048>().unwrap();
    raw_sched_getaffinity::<0x1000>().unwrap();
    raw_sched_getaffinity::<0x2000>().unwrap();
    raw_sched_getaffinity::<0x4000>().unwrap();
    raw_sched_getaffinity::<0x8000>().unwrap();
}
