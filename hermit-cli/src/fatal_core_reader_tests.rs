/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! [`crate::fatal_core_registers`] on a kernel without `PIDFD_THREAD`.

use safeptrace::Pid;
use safeptrace::Stopped;

/// Kills and reaps the traced child on every exit from the test.
struct Child(libc::pid_t);

impl Drop for Child {
    fn drop(&mut self) {
        // SAFETY: kill and waitpid take plain values and a local status.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
            loop {
                let mut status = 0;
                let reaped = libc::waitpid(self.0, &mut status, libc::__WALL);
                if reaped == self.0 && (libc::WIFEXITED(status) || libc::WIFSIGNALED(status)) {
                    break;
                }
                if reaped < 0 && *libc::__errno_location() != libc::EINTR {
                    break;
                }
            }
        }
    }
}

/// Makes `pidfd_open(_, PIDFD_THREAD)` fail with `EINVAL` on this thread, as
/// it does on kernels before Linux 6.9, and leaves every other call alone.
/// `PIDFD_THREAD` is `O_EXCL`.
fn refuse_thread_pidfds() {
    let instruction = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let filter = [
        // The system call number.
        instruction(0x20, 0, 0, 0),
        instruction(0x15, 0, 3, libc::SYS_pidfd_open as u32),
        // The low word of its second argument, the flags.
        instruction(0x20, 0, 0, 24),
        instruction(0x15, 0, 1, libc::O_EXCL as u32),
        instruction(0x06, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EINVAL as u32),
        instruction(0x06, 0, 0, libc::SECCOMP_RET_ALLOW),
    ];
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: prctl and seccomp with a valid filter program that outlives
    // both calls.
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        assert_eq!(
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                0,
                &program
            ),
            0
        );
    }
}

/// A thread held at a stop whose register write still waits in safeptrace's
/// cache, as Reverie's backend leaves a thread Detcore parks after running a
/// syscall for it, reaches a fatal core with the cached registers, not the
/// kernel's, even where the kernel refuses thread pidfds. Reverie then holds
/// its threads through ptracer-thread capabilities, and the reader must join
/// them rather than mint a capability bound to no generation.
#[test]
fn a_held_threads_cached_registers_reach_the_core_without_thread_pidfds() {
    if crate::run_in_fresh_process(
        "fatal_core_reader_tests::a_held_threads_cached_registers_reach_the_core_without_thread_pidfds",
    ) {
        return;
    }
    // SAFETY: the child makes only async-signal-safe calls before _exit.
    let raw = unsafe { libc::fork() };
    assert!(raw >= 0);
    if raw == 0 {
        // SAFETY: async-signal-safe calls in the forked child.
        unsafe {
            if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) != 0 || libc::raise(libc::SIGSTOP) != 0 {
                libc::_exit(2);
            }
            libc::_exit(0);
        }
    }
    let _child = Child(raw);
    let mut status = 0;
    // SAFETY: waitpid on our own child with a local status.
    assert_eq!(unsafe { libc::waitpid(raw, &mut status, 0) }, raw);
    assert!(libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGSTOP);

    refuse_thread_pidfds();
    // SAFETY: pidfd_open and close take plain values.
    unsafe {
        assert_eq!(libc::syscall(libc::SYS_pidfd_open, raw, libc::O_EXCL), -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL),
            "a thread pidfd is refused"
        );
        let ordinary = libc::syscall(libc::SYS_pidfd_open, raw, 0);
        assert!(ordinary >= 0, "an ordinary pidfd still opens");
        libc::close(ordinary as i32);
    }

    let pid = Pid::from_raw(raw);
    let tracer = Stopped::new_unchecked_on_ptracer_thread(pid).unwrap();
    let kernel = tracer.getregs().unwrap();
    let mut cached = kernel;
    cached.rip ^= 0x1000;
    cached.rcx ^= 0x1000;
    tracer.setregs(&cached).unwrap();
    let mut held = std::mem::MaybeUninit::<libc::user_regs_struct>::uninit();
    // SAFETY: PTRACE_GETREGS of a thread this thread traces and holds, into
    // a local buffer of the right size.
    let held = unsafe {
        assert_eq!(
            libc::ptrace(libc::PTRACE_GETREGS, raw, 0, held.as_mut_ptr()),
            0
        );
        held.assume_init()
    };
    assert_eq!(
        (held.rip, held.rcx),
        (kernel.rip, kernel.rcx),
        "the write still waits in the cache"
    );

    let core = crate::fatal_core_registers(raw).expect("the reader reads a held thread");
    assert_eq!(
        (core.rip, core.rcx),
        (cached.rip, cached.rcx),
        "the core holds the registers the tracer holds"
    );
}
