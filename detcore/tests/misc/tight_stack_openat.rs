/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression coverage for <https://github.com/rrnewton/hermit/issues/3328>.
//!
//! After every successful `openat`, Detcore injects an `fstat` to record the
//! new descriptor's metadata. The ptrace backend used to stage that `fstat`
//! buffer in the guest's stack below `rsp` and turned a failed staging write
//! into the result of the guest's `openat`. A guest whose stack pointer sits
//! close to the end of its writable stack then saw a successful open reported
//! as `EFAULT`, while the real descriptor stayed open. That stopped the glibc
//! dynamic loader of three real test binaries before `main`.
//!
//! The guest here issues one raw `openat` with its stack pointer a chosen
//! number of bytes above memory it cannot write, and requires the result
//! Linux gives natively: the lowest free descriptor, usable, and not leaked.

use std::ffi::CStr;
use std::mem::MaybeUninit;

const PATH: &CStr = c"/dev/null";

/// What lies directly below the writable stack bytes.
#[derive(Clone, Copy, Debug)]
enum BelowStack {
    /// A `PROT_NONE` guard page, as below a thread or fiber stack.
    GuardPage,
    /// Nothing mapped, as below the lowest page of the main-thread stack VMA,
    /// which a remote (tracer) write does not grow.
    Unmapped,
}

/// Issue `openat(AT_FDCWD, PATH, O_RDONLY | O_CLOEXEC)` with `rsp` switched to
/// `stack` for the duration of the `syscall` instruction.
///
/// # Safety
///
/// `stack` must be a writable address; nothing is pushed to it, but a signal
/// delivered during the syscall would run its handler on it.
unsafe fn raw_openat_on_stack(stack: *mut u8, path: &CStr) -> i64 {
    let result: i64;
    // SAFETY: rsp is swapped with r12 and swapped back before the block ends;
    // `syscall` itself does not touch the user stack. rcx and r11 are the
    // registers `syscall` clobbers.
    unsafe {
        std::arch::asm!(
            "xchg rsp, r12",
            "syscall",
            "xchg rsp, r12",
            inout("r12") stack => _,
            inlateout("rax") libc::SYS_openat => result,
            in("rdi") i64::from(libc::AT_FDCWD),
            in("rsi") path.as_ptr(),
            in("rdx") i64::from(libc::O_RDONLY | libc::O_CLOEXEC),
            in("r10") 0_i64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn fstat(fd: i32) -> libc::stat {
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    assert_eq!(
        unsafe { libc::fstat(fd, stat.as_mut_ptr()) },
        0,
        "fstat({fd})"
    );
    unsafe { stat.assume_init() }
}

/// Map two pages, leave only the upper one writable, and return the address
/// `writable_bytes` above its lower end together with the mapping to release.
fn tight_stack(below: BelowStack, writable_bytes: usize) -> (*mut u8, *mut libc::c_void, usize) {
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();
    assert!(writable_bytes <= page);
    let region = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            2 * page,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(region, libc::MAP_FAILED, "stack region mmap");
    let upper = unsafe { region.cast::<u8>().add(page) };
    assert_eq!(
        unsafe { libc::mprotect(upper.cast(), page, libc::PROT_READ | libc::PROT_WRITE) },
        0,
        "mprotect of the writable stack page"
    );
    let (mapping, mapping_len) = match below {
        BelowStack::GuardPage => (region, 2 * page),
        BelowStack::Unmapped => {
            assert_eq!(unsafe { libc::munmap(region, page) }, 0, "unmap lower page");
            (upper.cast(), page)
        }
    };
    (unsafe { upper.add(writable_bytes) }, mapping, mapping_len)
}

#[test]
fn openat_succeeds_without_writable_stack_below_rsp() {
    super::det_test_fn_sequential_without_pmu(|| {
        // The descriptor and metadata an ordinary open of the same file yields.
        let expected = unsafe { libc::open(PATH.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        assert!(expected >= 0, "ordinary open of {PATH:?} failed");
        let expected_stat = fstat(expected);
        assert_eq!(unsafe { libc::close(expected) }, 0);

        // 512 bytes leaves room for the whole scratch area and is the control;
        // 192 is the probe value from the issue; 0 puts rsp at the boundary.
        for (below, writable_bytes) in [
            (BelowStack::GuardPage, 512),
            (BelowStack::GuardPage, 192),
            (BelowStack::GuardPage, 0),
            (BelowStack::Unmapped, 192),
        ] {
            let (stack, mapping, mapping_len) = tight_stack(below, writable_bytes);
            let fd = unsafe { raw_openat_on_stack(stack, PATH) };
            assert_eq!(
                fd,
                i64::from(expected),
                "raw openat with {writable_bytes} writable bytes above a {below:?} region \
                 returned {fd}; natively it returns the lowest free descriptor {expected}"
            );
            let fd = i32::try_from(fd).unwrap();
            let stat = fstat(fd);
            assert_eq!(
                (stat.st_mode, stat.st_rdev, stat.st_ino),
                (
                    expected_stat.st_mode,
                    expected_stat.st_rdev,
                    expected_stat.st_ino
                ),
                "descriptor opened on a tight stack ({below:?}, {writable_bytes}) names a \
                 different file"
            );
            let mut byte = 0_u8;
            assert_eq!(
                unsafe { libc::read(fd, (&raw mut byte).cast(), 1) },
                0,
                "read from {PATH:?}"
            );
            assert_eq!(unsafe { libc::close(fd) }, 0);
            assert_eq!(unsafe { libc::munmap(mapping, mapping_len) }, 0);
        }

        // Nothing was left open behind the guest's back.
        let again = unsafe { libc::open(PATH.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        assert_eq!(again, expected, "a descriptor leaked");
        assert_eq!(unsafe { libc::close(again) }, 0);
    });
}
