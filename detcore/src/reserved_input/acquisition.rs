/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Native descriptor publication policy. These classes describe an actual
//! application attempt, never evidence that a matched Tool handler ran it.

use reverie::syscalls::Errno;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

/// The membership evidence required after a physically issued application call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AcquisitionClass {
    /// Kernel construction creates fresh descriptions, including partial
    /// publication before an output error. No existing OFD state is imported.
    Fresh,
    /// One installed descriptor is the successful native return value. Errors
    /// from these syscall implementations precede publication of that result.
    ReturnedFd,
    /// Returned payload/message counts do not enumerate installed descriptors.
    /// A native error can follow ancillary installation and output faults.
    Received,
    /// A supported ioctl can install descriptors. Its command is not an OFD
    /// identity proof, and its generic return/error is not a membership list.
    IoctlImport,
}

impl AcquisitionClass {
    /// This answers only whether the syscall's ordinary descriptor publication
    /// is absent. It makes no claim about arbitrary driver-internal effects.
    /// In particular, Err(EFAULT) from recvmsg is NOT a no-install proof:
    /// Linux net/socket.c ____sys_recvmsg copies the address/flags/controllen
    /// after sock_recvmsg has already delivered SCM_RIGHTS.
    pub(super) fn native_error_proves_no_install(self, result: Result<i64, Errno>) -> bool {
        self == Self::ReturnedFd && result.is_err()
    }

    pub(super) fn needs_membership(self) -> bool {
        self != Self::Fresh
    }
}

/// Only native ABI calls reach this classifier. Unknown driver operations are
/// not certified safe by returning None: they retain the separately accepted
/// S5 scope and their direct-reference policy. In particular this is not an
/// all-ioctl permanent taint.
pub(super) fn classify(call: Syscall) -> Option<AcquisitionClass> {
    use AcquisitionClass::*;
    let (_, args) = call.into_parts();
    match call.name() {
        // The kernel publishes files allocated by these operations, not the
        // listening socket's description or whichever FD a prior probe saw.
        "socket" | "socketpair" | "pipe" | "pipe2" | "accept" | "accept4" => Some(Fresh),
        // Linux fs/open.c do_sys_openat2 and fs/file.c dup/f_dupfd/ksys_dup3:
        // the descriptor is returned only after publication; ordinary error
        // paths do not publish it. pidfd_getfd uses receive_fd with no user
        // output pointer and returns that same published FD.
        "open" | "openat" | "openat2" | "creat" | "open_by_handle_at" | "dup" | "dup3"
        | "pidfd_getfd" | "memfd_create" | "eventfd" | "eventfd2" | "timerfd_create"
        | "inotify_init" | "inotify_init1" | "epoll_create" | "epoll_create1" | "userfaultfd"
        | "pidfd_open" => Some(ReturnedFd),
        // dup2(old, old) only validates that existing slot; it does not acquire
        // or replace a description. Its actual native result is unchanged.
        "dup2" if args.arg0 as i32 != args.arg1 as i32 => Some(ReturnedFd),
        "fcntl" if matches!(args.arg1 as i32, libc::F_DUPFD | libc::F_DUPFD_CLOEXEC) => {
            Some(ReturnedFd)
        }
        // An existing signalfd is reconfigured without installing another FD.
        "signalfd" | "signalfd4" if (args.arg0 as i32) < 0 => Some(ReturnedFd),
        // Negative FDs cannot reach socket receive. Zero vlen cannot deliver
        // any message. Zero *payload bytes* from recvmsg is not such a proof.
        "recvmsg" if (args.arg0 as i32) >= 0 => Some(Received),
        "recvmmsg" if (args.arg0 as i32) >= 0 && args.arg2 as u32 != 0 => Some(Received),
        "ioctl" if (args.arg0 as i32) >= 0 && descriptor_ioctl(args.arg1 as u32) => {
            Some(IoctlImport)
        }
        _ => None,
    }
}

fn descriptor_ioctl(command: u32) -> bool {
    // Full native ioctl command, not the low command byte. These are Linux
    // binder.h BINDER_WRITE_READ, asm-generic/ioctls.h TIOCGPTPEER, and
    // nsfs.h NS_GET_USERNS / NS_GET_PARENT and the extensible NEXT/PREV
    // namespace commands. Actual object classification and
    // complete installed-membership proof remain required at the native stop.
    matches!(command, 0xc030_6201 | 0x5441 | 0xb701 | 0xb702)
        || (matches!(command & !0x3fff_0000, 0x8000_b70b | 0x8000_b70c)
            && (command >> 16) & 0x3fff >= 16)
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::*;

    fn call(number: Sysno, args: [usize; 6]) -> Syscall {
        Syscall::from_raw(
            number,
            SyscallArgs::new(args[0], args[1], args[2], args[3], args[4], args[5]),
        )
    }

    #[test]
    fn native_receive_error_and_zero_payload_do_not_prove_empty_membership() {
        let receive = classify(call(Sysno::recvmsg, [7, 0, 0, 0, 0, 0])).unwrap();
        assert_eq!(receive, AcquisitionClass::Received);
        for result in [Ok(0), Ok(1), Err(Errno::EFAULT), Err(Errno::EAGAIN)] {
            assert!(!receive.native_error_proves_no_install(result));
        }
        assert_eq!(
            classify(call(Sysno::recvmsg, [usize::MAX, 0, 0, 0, 0, 0])),
            None
        );
        assert_eq!(classify(call(Sysno::recvmmsg, [7, 1, 0, 0, 0, 0])), None);
        let open = classify(call(Sysno::openat, [0; 6])).unwrap();
        assert!(open.native_error_proves_no_install(Err(Errno::EFAULT)));
        assert!(!open.native_error_proves_no_install(Ok(7)));
    }

    #[test]
    fn acquisition_commands_do_not_taint_unrelated_ioctl_or_existing_signalfd() {
        for command in [
            0xc030_6201,
            0x5441,
            0xb701,
            0xb702,
            0x8010_b70b,
            0x8020_b70c,
        ] {
            assert_eq!(
                classify(call(Sysno::ioctl, [7, command, 0, 0, 0, 0])),
                Some(AcquisitionClass::IoctlImport)
            );
        }
        for command in [
            libc::FIONBIO as usize,
            libc::FIONREAD as usize,
            0x6201,
            0xc020_6201,
            0x800f_b70b,
            0x4010_b70b,
            0x8010_b70a,
        ] {
            assert_eq!(classify(call(Sysno::ioctl, [7, command, 0, 0, 0, 0])), None);
        }
        assert_eq!(classify(call(Sysno::signalfd4, [7, 0, 8, 0, 0, 0])), None);
        assert_eq!(
            classify(call(Sysno::signalfd4, [usize::MAX, 0, 8, 0, 0, 0])),
            Some(AcquisitionClass::ReturnedFd)
        );
    }

    #[test]
    fn native_fresh_results_keep_the_kernel_contract_without_listener_snapshot() {
        for number in [
            Sysno::socket,
            Sysno::socketpair,
            Sysno::pipe2,
            Sysno::accept4,
        ] {
            let class = classify(call(number, [0; 6])).unwrap();
            assert_eq!(class, AcquisitionClass::Fresh);
            assert!(!class.needs_membership());
        }
        assert!(
            classify(call(Sysno::openat, [0; 6]))
                .unwrap()
                .needs_membership()
        );
        assert!(
            classify(call(Sysno::recvmsg, [7, 0, 0, 0, 0, 0]))
                .unwrap()
                .needs_membership()
        );
    }
}
