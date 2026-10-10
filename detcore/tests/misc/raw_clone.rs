/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A `clone` without `CLONE_THREAD` whose exit signal is not SIGCHLD makes a
//! process, although ptrace reports it as a clone event. Its creator's exit
//! must not wait for it as for a thread
//! (https://github.com/rrnewton/hermit/issues/4012).

use std::ffi::c_void;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

const CHILD_YIELDS: usize = 200;

extern "C" fn ignore_signal(_signal: libc::c_int) {}

extern "C" fn yield_then_exit(_arg: *mut c_void) -> libc::c_int {
    for _ in 0..CHILD_YIELDS {
        unsafe { libc::sched_yield() };
    }
    0
}

/// Forks a middle process that raw-clones a child with `flags` and exits 5
/// while the child still runs. Returns what the caller's wait for the middle
/// process saw.
fn middle_exit_status_with_live_raw_clone(flags: libc::c_int) -> (bool, libc::c_int) {
    let middle = unsafe { libc::fork() };
    assert!(middle >= 0, "fork failed");
    if middle == 0 {
        let mut stack = vec![0_u8; 64 * 1024];
        let stack_top = unsafe { stack.as_mut_ptr().add(stack.len()) }.cast::<c_void>();
        let child = unsafe { libc::clone(yield_then_exit, stack_top, flags, std::ptr::null_mut()) };
        if child < 0 {
            unsafe { libc::_exit(3) };
        }
        // The stack stays mapped for a CLONE_VM child: _exit runs no
        // destructors, and the child is its own thread group, so exit_group
        // does not end it.
        std::mem::forget(stack);
        unsafe { libc::_exit(5) };
    }
    let mut status = 0;
    let reaped = unsafe { libc::waitpid(middle, &mut status, 0) };
    (reaped == middle, status)
}

#[test]
fn a_live_raw_clone_child_does_not_hold_its_creators_exit() {
    super::det_test_fn_sequential_without_pmu(|| {
        let (reaped, status) = middle_exit_status_with_live_raw_clone(libc::SIGUSR1);
        assert!(reaped, "waitpid did not return the middle process");
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 5);
    });
}

#[test]
fn a_live_raw_clone_child_sharing_memory_does_not_hold_its_creators_exit() {
    super::det_test_fn_sequential_without_pmu(|| {
        let (reaped, status) =
            middle_exit_status_with_live_raw_clone(libc::CLONE_VM | libc::SIGUSR1);
        assert!(reaped, "waitpid did not return the middle process");
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 5);
    });
}

#[test]
fn a_raw_clone_child_is_waited_for_as_a_process() {
    super::det_test_fn_sequential_without_pmu(|| {
        // The child's exit signal is SIGUSR1, whose default action would end
        // this process when the child exits.
        let handler = ignore_signal as extern "C" fn(libc::c_int);
        assert_ne!(
            unsafe { libc::signal(libc::SIGUSR1, handler as libc::sighandler_t) },
            libc::SIG_ERR
        );
        let mut stack = vec![0_u8; 64 * 1024];
        let stack_top = unsafe { stack.as_mut_ptr().add(stack.len()) }.cast::<c_void>();
        let child = unsafe {
            libc::clone(
                yield_then_exit,
                stack_top,
                libc::SIGUSR1,
                std::ptr::null_mut(),
            )
        };
        assert!(child > 0, "clone(SIGUSR1) failed");
        // A non-SIGCHLD child is waited for with __WCLONE (or __WALL).
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(child, &mut status, libc::__WCLONE) },
            child
        );
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        drop(stack);
    });
}

/// The middle process's raw-clone child outlives it, and the caller is a
/// child subreaper: the caller reaps the middle process at once, adopts the
/// raw-clone child (Linux resets its exit signal to SIGCHLD), sees it live
/// with WNOHANG, reaps it with a plain wait once released, and then gets
/// ECHILD (https://github.com/rrnewton/hermit/issues/4012).
#[test]
fn a_subreaper_reaps_the_creator_and_adopts_its_live_raw_clone_child() {
    super::det_test_fn_sequential_without_pmu(|| {
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0), 0);
        }
        let mut release = [0; 2];
        let mut report = [0; 2];
        assert_eq!(unsafe { libc::pipe(release.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(report.as_mut_ptr()) }, 0);
        let middle = unsafe { libc::fork() };
        assert!(middle >= 0, "fork failed");
        if middle == 0 {
            unsafe {
                libc::close(report[0]);
                let child =
                    libc::syscall(libc::SYS_clone, libc::SIGUSR1 as libc::c_ulong, 0, 0, 0, 0);
                if child == 0 {
                    libc::close(release[1]);
                    let mut byte = 0_u8;
                    let read = libc::read(release[0], (&mut byte as *mut u8).cast(), 1);
                    libc::_exit(if read == 1 { 9 } else { 14 });
                }
                let child = child as libc::pid_t;
                let wrote = libc::write(
                    report[1],
                    (&child as *const libc::pid_t).cast(),
                    std::mem::size_of::<libc::pid_t>(),
                );
                libc::_exit(if wrote == 4 { 0 } else { 15 });
            }
        }
        unsafe {
            libc::close(report[1]);
            libc::close(release[0]);
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(middle, &mut status, 0) }, middle);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        let mut child: libc::pid_t = 0;
        let read = unsafe {
            libc::read(
                report[0],
                (&mut child as *mut libc::pid_t).cast(),
                std::mem::size_of::<libc::pid_t>(),
            )
        };
        assert_eq!(read, 4);
        assert_eq!(
            unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) },
            0,
            "the adopted child is live"
        );
        assert_eq!(
            unsafe { libc::write(release[1], b"x".as_ptr().cast(), 1) },
            1
        );
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 9);
        assert_eq!(unsafe { libc::waitpid(-1, &mut status, 0) }, -1);
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::ECHILD);
    });
}

static SIGCHLD_COUNT: AtomicI32 = AtomicI32::new(0);
static SIGUSR1_COUNT: AtomicI32 = AtomicI32::new(0);

extern "C" fn count_signal(signal: libc::c_int) {
    match signal {
        libc::SIGCHLD => SIGCHLD_COUNT.fetch_add(1, Ordering::SeqCst),
        libc::SIGUSR1 => SIGUSR1_COUNT.fetch_add(1, Ordering::SeqCst),
        _ => 0,
    };
}

extern "C" fn exit_group_seven(_arg: *mut c_void) -> libc::c_int {
    unsafe { libc::syscall(libc::SYS_exit_group, 7) };
    unreachable!()
}

/// A raw clone child ends with exit_group, as `_exit`, `exit` and returning
/// from `main` do. Its parent is notified with the child's own exit signal
/// only: none for 0, SIGUSR1 for SIGUSR1, SIGCHLD only for SIGCHLD
/// (https://github.com/rrnewton/hermit/issues/4012).
#[test]
fn a_raw_clone_child_ending_with_exit_group_notifies_with_its_own_exit_signal() {
    super::det_test_fn_sequential_without_pmu(|| {
        let handler = count_signal as extern "C" fn(libc::c_int);
        for signal in [libc::SIGCHLD, libc::SIGUSR1] {
            assert_ne!(
                unsafe { libc::signal(signal, handler as libc::sighandler_t) },
                libc::SIG_ERR
            );
        }
        for exit_signal in [0, libc::SIGUSR1, libc::SIGCHLD] {
            SIGCHLD_COUNT.store(0, Ordering::SeqCst);
            SIGUSR1_COUNT.store(0, Ordering::SeqCst);
            let mut stack = vec![0_u8; 64 * 1024];
            let stack_top = unsafe { stack.as_mut_ptr().add(stack.len()) }.cast::<c_void>();
            let child = unsafe {
                libc::clone(
                    exit_group_seven,
                    stack_top,
                    exit_signal,
                    std::ptr::null_mut(),
                )
            };
            assert!(child > 0, "clone failed");
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(child, &mut status, libc::__WALL) },
                child
            );
            assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 7);
            for _ in 0..20 {
                unsafe { libc::sched_yield() };
            }
            let expected = match exit_signal {
                libc::SIGCHLD => (1, 0),
                libc::SIGUSR1 => (0, 1),
                _ => (0, 0),
            };
            assert_eq!(
                (
                    SIGCHLD_COUNT.load(Ordering::SeqCst),
                    SIGUSR1_COUNT.load(Ordering::SeqCst)
                ),
                expected,
                "exit signal {exit_signal}: (SIGCHLD, SIGUSR1) handled"
            );
            drop(stack);
        }
    });
}
