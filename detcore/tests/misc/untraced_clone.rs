/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A `clone` or `clone3` with `CLONE_UNTRACED` makes a child that runs as it
//! does natively. Hermit must still trace it, and clears the flag in the call
//! it makes; before, the child ran untraced, Detcore waited for it to take
//! its first turn, and the clone never returned. LeakSanitizer's exit-time
//! check makes such a clone, so every run of a sanitizer build that checked
//! for leaks hung at exit.

use std::ffi::c_void;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

static CHILD_VALUE: AtomicI32 = AtomicI32::new(0);
/// The word a `CLONE_CHILD_CLEARTID` thread clears, and wakes, as it exits.
static CLEAR_TID: AtomicI32 = AtomicI32::new(0);

extern "C" fn store_and_return(value: *mut c_void) -> libc::c_int {
    CHILD_VALUE.store(value as usize as i32, Ordering::SeqCst);
    0
}

extern "C" fn store_and_exit_thread(value: *mut c_void) -> libc::c_int {
    CHILD_VALUE.store(value as usize as i32, Ordering::SeqCst);
    unsafe { libc::syscall(libc::SYS_exit, 0) };
    unreachable!("exit returned")
}

/// A stack for a child, as a leaked allocation: a child that shares this
/// memory may still run on it after the caller returns.
fn child_stack() -> *mut c_void {
    let stack = Box::leak(vec![0_u8; 64 * 1024].into_boxed_slice());
    unsafe { stack.as_mut_ptr().add(stack.len()) }.cast()
}

/// Waits for `child` with `options` and returns its exit code.
fn exit_code(child: libc::pid_t, options: libc::c_int) -> i32 {
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, options) }, child);
    assert!(libc::WIFEXITED(status), "status {status:#x}");
    libc::WEXITSTATUS(status)
}

/// The shape of LeakSanitizer's helper (compiler-rt `StopTheWorld`): a child
/// that shares memory, files and fs but is not a thread, with no exit
/// signal, waited for with `__WALL`.
#[test]
fn an_untraced_helper_sharing_memory_runs_and_is_waited_for() {
    super::det_test_fn_sequential_without_pmu(|| {
        CHILD_VALUE.store(0, Ordering::SeqCst);
        let flags = libc::CLONE_VM | libc::CLONE_FS | libc::CLONE_FILES | libc::CLONE_UNTRACED;
        let child =
            unsafe { libc::clone(store_and_return, child_stack(), flags, 42 as *mut c_void) };
        assert!(
            child > 0,
            "clone failed: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(exit_code(child, libc::__WALL), 0);
        assert_eq!(CHILD_VALUE.load(Ordering::SeqCst), 42);
    });
}

/// A fork-like child, reaped with a plain wait.
#[test]
fn an_untraced_process_exits_and_is_reaped() {
    super::det_test_fn_sequential_without_pmu(|| {
        let flags = (libc::CLONE_UNTRACED | libc::SIGCHLD) as libc::c_ulong;
        let child = unsafe { libc::syscall(libc::SYS_clone, flags, 0, 0, 0, 0) };
        if child == 0 {
            unsafe { libc::_exit(7) };
        }
        assert!(
            child > 0,
            "clone failed: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(exit_code(child as libc::pid_t, 0), 7);
    });
}

/// A thread, joined through `CLONE_CHILD_CLEARTID`.
#[test]
fn an_untraced_thread_runs_and_is_joined() {
    super::det_test_fn_sequential_without_pmu(|| {
        CHILD_VALUE.store(0, Ordering::SeqCst);
        CLEAR_TID.store(1, Ordering::SeqCst);
        let flags = libc::CLONE_THREAD
            | libc::CLONE_VM
            | libc::CLONE_SIGHAND
            | libc::CLONE_FS
            | libc::CLONE_FILES
            | libc::CLONE_SYSVSEM
            | libc::CLONE_CHILD_CLEARTID
            | libc::CLONE_UNTRACED;
        let child = unsafe {
            libc::clone(
                store_and_exit_thread,
                child_stack(),
                flags,
                43 as *mut c_void,
                std::ptr::null_mut::<libc::pid_t>(),
                std::ptr::null_mut::<c_void>(),
                CLEAR_TID.as_ptr(),
            )
        };
        assert!(
            child > 0,
            "clone failed: {}",
            std::io::Error::last_os_error()
        );
        loop {
            let word = CLEAR_TID.load(Ordering::SeqCst);
            if word == 0 {
                break;
            }
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    CLEAR_TID.as_ptr(),
                    libc::FUTEX_WAIT,
                    word,
                    std::ptr::null::<libc::timespec>(),
                )
            };
        }
        assert_eq!(CHILD_VALUE.load(Ordering::SeqCst), 43);
    });
}

/// The size of `clone3`'s `struct clone_args`, eleven 8-byte fields
/// (`CLONE_ARGS_SIZE_VER2`, include/uapi/linux/sched.h): flags, pidfd,
/// child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid,
/// set_tid_size and cgroup.
const CLONE_ARGS_SIZE: usize = 88;

/// `CLONE_INTO_CGROUP`, a `clone3`-only flag (include/uapi/linux/sched.h).
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// `clone3` of a fork-like child that exits 7, with `args` of `size` bytes,
/// whose first [`CLONE_ARGS_SIZE`] are a `struct clone_args`. Returns what `clone3` returned to the
/// caller, or `errno` as a negative value, and the reaped child's exit code.
fn untraced_clone3(args: &mut [u64], size: usize) -> (i64, Option<i32>) {
    let child = unsafe { libc::syscall(libc::SYS_clone3, args.as_mut_ptr(), size) };
    if child == 0 {
        unsafe { libc::_exit(7) };
    }
    if child < 0 {
        return (
            -i64::from(std::io::Error::last_os_error().raw_os_error().unwrap()),
            None,
        );
    }
    (child, Some(exit_code(child as libc::pid_t, 0)))
}

/// `clone3` with `CLONE_UNTRACED` makes its child, and leaves the caller's
/// `clone_args` as it was: hermit clears the flag in a copy of its own.
#[test]
fn clone3_untraced_makes_its_child_and_leaves_its_arguments() {
    super::det_test_fn_sequential_without_pmu(|| {
        let mut args = [0_u64; 11];
        args[0] = libc::CLONE_UNTRACED as u64;
        args[4] = libc::SIGCHLD as u64;
        let (child, code) = untraced_clone3(&mut args, CLONE_ARGS_SIZE);
        assert!(child > 0, "clone3 failed: {child}");
        assert_eq!(code, Some(7));
        assert_eq!(
            args[0],
            libc::CLONE_UNTRACED as u64,
            "the caller's flags changed"
        );
    });
}

/// A `clone_args` longer than the kernel's is accepted when the bytes beyond
/// it are zero, and refused with `E2BIG` when they are not, as natively.
#[test]
fn clone3_untraced_takes_a_longer_clone_args_as_linux_does() {
    super::det_test_fn_sequential_without_pmu(|| {
        let mut args = [0_u64; 16];
        args[0] = libc::CLONE_UNTRACED as u64;
        args[4] = libc::SIGCHLD as u64;
        let (child, code) = untraced_clone3(&mut args, 128);
        assert!(child > 0, "clone3 failed: {child}");
        assert_eq!(code, Some(7));
        args[12] = 1;
        assert_eq!(
            untraced_clone3(&mut args, 128),
            (-i64::from(libc::E2BIG), None)
        );
    });
}

/// `clone3` of a vfork child on the caller's own stack (`CLONE_VM |
/// CLONE_VFORK`, no stack), where hermit stages its copy of the arguments:
/// the child exits 5 at once, without touching memory, and the caller,
/// resumed, reaps it.
#[cfg(target_arch = "x86_64")]
#[test]
fn clone3_untraced_vfork_runs_on_the_callers_stack() {
    super::det_test_fn_sequential_without_pmu(|| {
        let mut args = [0_u64; 11];
        args[0] = (libc::CLONE_VM | libc::CLONE_VFORK | libc::CLONE_UNTRACED) as u64;
        args[4] = libc::SIGCHLD as u64;
        let child: i64;
        // The child runs on this stack until it exits, so it makes its exit
        // call in the same instructions, with no call or return.
        unsafe {
            std::arch::asm!(
                "syscall",
                "test rax, rax",
                "jnz 2f",
                "mov eax, {exit}",
                "mov edi, 5",
                "syscall",
                "2:",
                exit = const libc::SYS_exit,
                inlateout("rax") libc::SYS_clone3 => child,
                in("rdi") args.as_mut_ptr(),
                in("rsi") CLONE_ARGS_SIZE,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
        assert!(child > 0, "clone3 failed: {child}");
        assert_eq!(exit_code(child as libc::pid_t, 0), 5);
    });
}

/// `clone3`'s pidfd for an untraced child is one Detcore knows: a SIGKILL
/// sent through it ends the child.
#[test]
fn clone3_untraced_pidfd_signals_its_child() {
    super::det_test_fn_sequential_without_pmu(|| {
        let mut pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let mut pidfd: libc::c_int = -1;
        let mut args = [0_u64; 11];
        args[0] = (libc::CLONE_PIDFD | libc::CLONE_UNTRACED) as u64;
        args[1] = (&mut pidfd as *mut libc::c_int) as u64;
        args[4] = libc::SIGCHLD as u64;
        let child = unsafe { libc::syscall(libc::SYS_clone3, args.as_mut_ptr(), CLONE_ARGS_SIZE) };
        if child == 0 {
            // Waits for a byte that never comes.
            let mut byte = 0_u8;
            unsafe { libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1) };
            unsafe { libc::_exit(3) };
        }
        assert!(
            child > 0,
            "clone3 failed: {}",
            std::io::Error::last_os_error()
        );
        assert!(pidfd >= 0, "no pidfd");
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd,
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        assert_eq!(
            sent,
            0,
            "pidfd_send_signal: {}",
            std::io::Error::last_os_error()
        );
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(child as libc::pid_t, &mut status, 0) },
            child as libc::pid_t
        );
        assert!(libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL);
    });
}

/// A `clone3` Linux refuses before it makes a child is refused as natively:
/// a size below the first `clone_args` (EINVAL), `CLONE_INTO_CGROUP` with a
/// size below the third (EINVAL), one above a page (E2BIG), no struct
/// (EFAULT). The smallest size Linux takes makes the child.
#[test]
fn clone3_untraced_sizes_and_faults_are_linuxs() {
    super::det_test_fn_sequential_without_pmu(|| {
        let mut args = [0_u64; 11];
        args[0] = libc::CLONE_UNTRACED as u64;
        args[4] = libc::SIGCHLD as u64;
        let (child, code) = untraced_clone3(&mut args, 64);
        assert!(child > 0, "clone3 failed: {child}");
        assert_eq!(code, Some(7));
        assert_eq!(
            untraced_clone3(&mut args, 63),
            (-i64::from(libc::EINVAL), None)
        );
        // CLONE_INTO_CGROUP needs the third version's size.
        args[0] |= CLONE_INTO_CGROUP;
        assert_eq!(
            untraced_clone3(&mut args, 64),
            (-i64::from(libc::EINVAL), None)
        );
        args[0] &= !CLONE_INTO_CGROUP;
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        assert_eq!(
            untraced_clone3(&mut args, page + 1),
            (-i64::from(libc::E2BIG), None)
        );
        let faulted = unsafe { libc::syscall(libc::SYS_clone3, 0_usize, CLONE_ARGS_SIZE) };
        assert_eq!(faulted, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EFAULT)
        );
    });
}
