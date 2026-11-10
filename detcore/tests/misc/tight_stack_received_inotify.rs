/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression coverage for the buffers Detcore injects syscalls with when a
//! guest receives an inotify descriptor and reads it.
//!
//! Detcore records a descriptor received over `SCM_RIGHTS` from the kernel's
//! facts about it. An inotify instance is an anonymous inode, which `fstat`
//! cannot tell from an eventfd, so Detcore also reads its link with an
//! injected `readlinkat`, whose path and result need a buffer in the guest.
//! A read of an inotify descriptor injects `FIONREAD`, whose result needs one
//! too. Each goes in the guest stack scratch, or, where the guest's stack
//! pointer sits just above memory it cannot write, in a transient page
//! (<https://github.com/rrnewton/hermit/issues/3328>). Without that, the
//! received descriptor would stay unrecorded and its rename cookies the
//! host's. The guest here receives an inotify instance and reads it with raw
//! syscalls issued with its stack pointer a chosen number of bytes above such
//! memory, and requires the rename cookie another instance sees.

use super::tight_stack_openat::BelowStack;
use super::tight_stack_openat::tight_stack;

/// Issue syscall `number` with three arguments, with `rsp` switched to
/// `stack` for the duration of the `syscall` instruction.
///
/// # Safety
///
/// `stack` must be a writable address; nothing is pushed to it, but a signal
/// delivered during the syscall would run its handler on it.
unsafe fn raw_syscall3_on_stack(stack: *mut u8, number: i64, args: [usize; 3]) -> i64 {
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
            inlateout("rax") number => result,
            in("rdi") args[0],
            in("rsi") args[1],
            in("rdx") args[2],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn watch(dir: &std::ffi::CStr) -> i32 {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    assert!(fd >= 0, "inotify_init1");
    let mask = libc::IN_MOVED_FROM | libc::IN_MOVED_TO;
    assert!(unsafe { libc::inotify_add_watch(fd, dir.as_ptr(), mask) } >= 0);
    fd
}

/// A `msghdr` for one byte of data and room for one descriptor.
struct Message {
    byte: u8,
    data: libc::iovec,
    control: [u64; 4],
    header: libc::msghdr,
}

impl Message {
    fn new() -> Box<Self> {
        let mut message = Box::new(Message {
            byte: 0,
            data: libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 1,
            },
            control: [0; 4],
            header: unsafe { std::mem::zeroed() },
        });
        message.data.iov_base = (&raw mut message.byte).cast();
        message.header.msg_iov = &raw mut message.data;
        message.header.msg_iovlen = 1;
        message.header.msg_control = message.control.as_mut_ptr().cast();
        message.header.msg_controllen =
            unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
        message
    }
}

/// The two events of one rename, as `fd` returns them: their cookies.
fn cookies(events: &[u8]) -> [u32; 2] {
    let cookie =
        |offset: usize| u32::from_ne_bytes(events[offset + 8..offset + 12].try_into().unwrap());
    [cookie(0), cookie(32)]
}

#[test]
fn a_received_inotify_descriptor_is_recorded_and_read_without_writable_stack_below_rsp() {
    super::det_test_fn_sequential_without_pmu(receive_and_read_on_tight_stacks);
}

fn receive_and_read_on_tight_stacks() {
    // 512 bytes leaves room for every scratch buffer and is the control; 0
    // puts rsp at the boundary.
    for (below, writable_bytes) in [
        (BelowStack::GuardPage, 512),
        (BelowStack::GuardPage, 0),
        (BelowStack::Unmapped, 64),
    ] {
        let mut template = *b"/tmp/received-inotify-XXXXXX\0";
        assert!(!unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null());
        let dir = std::ffi::CStr::from_bytes_with_nul(&template).unwrap();
        let path = |name: &str| {
            std::ffi::CString::new(format!("{}/{name}", dir.to_str().unwrap())).unwrap()
        };
        let first = path("f0");
        let fd = unsafe {
            libc::open(
                first.as_ptr(),
                libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC,
                0o644,
            )
        };
        assert!(fd >= 0 && unsafe { libc::close(fd) } == 0);

        let reference = watch(dir);
        let sent = watch(dir);
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let outgoing = Message::new();
        let header = unsafe { libc::CMSG_FIRSTHDR(&raw const outgoing.header) };
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<i32>(), sent);
        }
        assert_eq!(
            unsafe { libc::sendmsg(pair[0], &raw const outgoing.header, 0) },
            1
        );
        assert_eq!(unsafe { libc::close(sent) }, 0);

        let (stack, mapping, mapping_len) = tight_stack(below, writable_bytes);
        let mut incoming = Message::new();
        let received = unsafe {
            raw_syscall3_on_stack(
                stack,
                libc::SYS_recvmsg,
                [
                    pair[1] as usize,
                    (&raw mut incoming.header) as usize,
                    libc::MSG_CMSG_CLOEXEC as usize,
                ],
            )
        };
        assert_eq!(
            received, 1,
            "recvmsg with {writable_bytes} writable bytes above a {below:?} region"
        );
        let header = unsafe { libc::CMSG_FIRSTHDR(&raw const incoming.header) };
        assert!(!header.is_null() && unsafe { (*header).cmsg_type } == libc::SCM_RIGHTS);
        let received = unsafe { std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<i32>()) };

        let second = path("f1");
        assert_eq!(unsafe { libc::rename(first.as_ptr(), second.as_ptr()) }, 0);
        let mut expected = [0_u8; 64];
        assert_eq!(
            unsafe { libc::read(reference, expected.as_mut_ptr().cast(), 64) },
            64
        );
        let mut events = [0_u8; 64];
        let length = unsafe {
            raw_syscall3_on_stack(
                stack,
                libc::SYS_read,
                [received as usize, events.as_mut_ptr() as usize, 64],
            )
        };
        assert_eq!(
            length, 64,
            "read with {writable_bytes} writable bytes above a {below:?} region"
        );
        let [from, to] = cookies(&events);
        assert!(
            from != 0 && from == to,
            "a rename's two events share one cookie"
        );
        assert_eq!(
            cookies(&events),
            cookies(&expected),
            "the received descriptor ({below:?}, {writable_bytes}) gives the rename another \
             instance's cookie"
        );

        for fd in [reference, received, pair[0], pair[1]] {
            assert_eq!(unsafe { libc::close(fd) }, 0);
        }
        assert_eq!(unsafe { libc::munmap(mapping, mapping_len) }, 0);
        assert_eq!(unsafe { libc::unlink(second.as_ptr()) }, 0);
        assert_eq!(unsafe { libc::rmdir(dir.as_ptr()) }, 0);
    }
}
