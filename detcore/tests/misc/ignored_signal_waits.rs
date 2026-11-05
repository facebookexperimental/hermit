/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Kernel-run poll-family waits and the signals the guest ignores.

use detcore::Config;
use detcore::Detcore;
use reverie::ExitStatus;

/// A 200 ms timeout in both forms, kept on a read-only page
/// (`read_only_timeouts`): Linux then cannot write back the time left, so a
/// `select`, `pselect6` or `ppoll` that a signal ends returns EINTR instead of
/// restarting (`poll_select_finish`, fs/select.c), and the guest sees it.
struct Timeouts {
    ts: libc::timespec,
    tv: libc::timeval,
}

fn read_only_timeouts() -> *const Timeouts {
    unsafe {
        let page = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        assert_ne!(page, libc::MAP_FAILED);
        let timeouts = page.cast::<Timeouts>();
        timeouts.write(Timeouts {
            ts: libc::timespec {
                tv_sec: 0,
                tv_nsec: 200_000_000,
            },
            tv: libc::timeval {
                tv_sec: 0,
                tv_usec: 200_000,
            },
        });
        assert_eq!(libc::mprotect(page, 4096, libc::PROT_READ), 0);
        timeouts
    }
}

/// The i-th wait of 200 ms on `fd`, none with a mask: a `poll`, a `ppoll`, a
/// `select`, a `pselect6` without a mask argument and one whose argument
/// names no mask (as glibc's `pselect` passes it).
fn wait_200ms(i: usize, fd: libc::c_int, timeouts: *const Timeouts) -> i64 {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let no_mask: [usize; 2] = [0, 8];
    unsafe {
        let ts = &raw const (*timeouts).ts;
        let tv = &raw const (*timeouts).tv;
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
        let set = &mut set as *mut libc::fd_set;
        let null = std::ptr::null_mut::<libc::fd_set>();
        match i % 5 {
            0 => libc::syscall(libc::SYS_poll, &mut pfd, 1, 200),
            1 => libc::syscall(libc::SYS_ppoll, &mut pfd, 1, ts, 0usize, 8),
            2 => libc::syscall(libc::SYS_select, fd + 1, set, null, null, tv),
            3 => libc::syscall(libc::SYS_pselect6, fd + 1, set, null, null, ts, 0usize),
            _ => libc::syscall(
                libc::SYS_pselect6,
                fd + 1,
                set,
                null,
                null,
                ts,
                no_mask.as_ptr(),
            ),
        }
    }
}

/// Linux discards a signal the process ignores when it is generated, so it
/// never ends a wait; ptrace queues it, and a poll-family wait that Detcore
/// runs in the kernel ended with a restart code. A `poll` then restarts
/// through `restart_syscall`, which Detcore does not support, and a
/// `select`, `pselect6` or `ppoll` whose timeout Linux cannot write back
/// returns EINTR. Without sequentialized threads each wait below runs in the
/// kernel while a child sends the guest SIGWINCH, ignored by default, every
/// 20 ms and then exits (SIGCHLD, ignored by default too); the guest waits
/// only once the child has started. Each wait must
/// time out, as it does natively, with the guest's mask as it was, and no
/// unsupported call may be met.
#[test]
fn kernel_run_poll_waits_time_out_through_ignored_signals() {
    let config = detcore_testutils::prepare_ptrace_timer_config(Config {
        sequentialize_threads: false,
        max_timeslice: None,
        ..Default::default()
    });
    let (output, mut state) = detcore_testutils::test_fn_with_config::<Detcore, _>(
        || {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            let mut started = [0; 2];
            assert_eq!(unsafe { libc::pipe(started.as_mut_ptr()) }, 0);
            let timeouts = read_only_timeouts();
            let mut results = Vec::new();
            for i in 0..5 {
                let parent = unsafe { libc::getpid() };
                let child = unsafe { libc::fork() };
                if child == 0 {
                    unsafe { libc::write(started[1], [0u8].as_ptr().cast(), 1) };
                    let gap = libc::timespec {
                        tv_sec: 0,
                        tv_nsec: 20_000_000,
                    };
                    for _ in 0..5 {
                        unsafe {
                            libc::nanosleep(&gap, std::ptr::null_mut());
                            libc::kill(parent, libc::SIGWINCH);
                        }
                    }
                    unsafe { libc::_exit(0) };
                }
                let mut byte = 0u8;
                assert_eq!(
                    unsafe { libc::read(started[0], (&raw mut byte).cast(), 1) },
                    1
                );
                results.push(wait_200ms(i, fds[0], timeouts));
                let mut status = 0;
                assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
            }
            let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe { libc::sigprocmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask) };
            let blocked = unsafe {
                libc::sigismember(&mask, libc::SIGWINCH) == 1
                    || libc::sigismember(&mask, libc::SIGCHLD) == 1
            };
            println!("{results:?} blocked: {blocked}");
        },
        detcore_testutils::prepare_ptrace_timer_config(config),
        true,
    )
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "guest failed: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout.trim(), "[0, 0, 0, 0, 0] blocked: false");
    assert_eq!(
        state.take_unsupported_syscalls(),
        Default::default(),
        "no call Detcore does not support"
    );
}
