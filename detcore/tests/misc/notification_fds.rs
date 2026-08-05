/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::CStr;
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::ptr;
use std::thread;

use detcore::Config;
use detcore::Detcore;
use reverie::ExitStatus;

const RUNS: usize = 5;
const MAX_ATTEMPTS: usize = 100_000;

fn run_five_times(guest: fn()) {
    run_five_times_with_expected_stdout_prefix(guest, None);
}

fn run_five_times_with_expected_stdout_prefix(guest: fn(), required_prefix: Option<&[u8]>) {
    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        ..Default::default()
    };
    let mut expected = None;

    // The first `test_fn_with_config` invocation initializes process-global
    // runner state. Validate its readiness result, but compare timestamps only
    // across the following independently executed guests, which all start from
    // the same initialized harness state.
    let first_run = usize::from(required_prefix.is_none());
    for run in first_run..=RUNS {
        let (output, _state) =
            detcore_testutils::test_fn_with_config::<Detcore, _>(guest, config.clone(), true)
                .unwrap_or_else(|error| panic!("notification guest run {run} failed: {error:#}"));
        assert_eq!(
            output.status,
            ExitStatus::Exited(0),
            "guest run {run} failed"
        );
        assert!(
            output.stderr.is_empty(),
            "guest run {run} wrote stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if let Some(required_prefix) = required_prefix {
            assert!(
                output.stdout.starts_with(required_prefix),
                "notification guest run {run} observed the wrong readiness"
            );
            let timestamp = std::str::from_utf8(&output.stdout[required_prefix.len()..])
                .expect("virtual timestamp should be UTF-8")
                .trim()
                .parse::<u128>()
                .expect("virtual timestamp should be an integer nanosecond value");
            assert!(timestamp > 0, "virtual timestamp should be positive");
        }

        if run == 0 {
            continue;
        }

        if let Some(expected) = &expected {
            assert_eq!(
                &output.stdout, expected,
                "notification output diverged on run {run}"
            );
        } else {
            expected = Some(output.stdout);
        }
    }

    if required_prefix.is_some() {
        println!(
            "stable guest output across {RUNS} runs: {}",
            String::from_utf8_lossy(expected.as_deref().expect("at least one guest run"))
        );
    }
}

fn close(fd: libc::c_int) {
    assert_eq!(unsafe { libc::close(fd) }, 0);
}

fn arm_timer() -> libc::c_int {
    let fd = unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
        )
    };
    assert!(fd >= 0, "timerfd_create failed: {}", errno());

    let value = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: 0,
            tv_nsec: 1,
        },
    };
    assert_eq!(
        unsafe { libc::timerfd_settime(fd, 0, &value, ptr::null_mut()) },
        0,
        "timerfd_settime failed: {}",
        errno()
    );
    fd
}

fn read_timer(fd: libc::c_int) -> u64 {
    for _ in 0..MAX_ATTEMPTS {
        let mut expirations = 0_u64;
        let result =
            unsafe { libc::read(fd, ptr::from_mut(&mut expirations).cast(), size_of::<u64>()) };
        if result == size_of::<u64>() as isize {
            return expirations;
        }
        assert_eq!(result, -1);
        assert_eq!(errno(), libc::EAGAIN);
        unsafe { libc::sched_yield() };
    }
    panic!("timerfd did not expire");
}

fn timerfd_guest() {
    let fd = arm_timer();
    println!("expirations={}", read_timer(fd));
    close(fd);
}

fn blocked_signal_set() -> libc::sigset_t {
    let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
    assert_eq!(unsafe { libc::sigemptyset(mask.as_mut_ptr()) }, 0);
    let mut mask = unsafe { mask.assume_init() };
    assert_eq!(unsafe { libc::sigaddset(&mut mask, libc::SIGUSR1) }, 0);
    assert_eq!(
        unsafe { libc::sigprocmask(libc::SIG_BLOCK, &mask, ptr::null_mut()) },
        0
    );
    mask
}

fn signal_fd() -> libc::c_int {
    let mask = blocked_signal_set();
    let fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
    assert!(fd >= 0, "signalfd failed: {}", errno());
    fd
}

fn signalfd_guest() {
    let fd = signal_fd();
    assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);
    let mut info = MaybeUninit::<libc::signalfd_siginfo>::uninit();
    assert_eq!(
        unsafe {
            libc::read(
                fd,
                info.as_mut_ptr().cast(),
                size_of::<libc::signalfd_siginfo>(),
            )
        },
        size_of::<libc::signalfd_siginfo>() as isize
    );
    let info = unsafe { info.assume_init() };
    println!("signal={}", info.ssi_signo);
    close(fd);
}

fn test_directory() -> (CString, Vec<CString>) {
    let directory = CString::new(format!("/tmp/hermit-notification-fds-{}", unsafe {
        libc::getpid()
    }))
    .unwrap();
    unsafe { libc::rmdir(directory.as_ptr()) };
    assert_eq!(unsafe { libc::mkdir(directory.as_ptr(), 0o700) }, 0);
    (directory, Vec::new())
}

fn watched_file(directory: &CStr, name: &str) -> CString {
    CString::new(format!("{}/{}", directory.to_string_lossy(), name)).unwrap()
}

fn create_file(directory: &CStr, name: &str) -> CString {
    let path = watched_file(directory, name);
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC | libc::O_CLOEXEC,
            0o600,
        )
    };
    assert!(fd >= 0, "open failed: {}", errno());
    assert_eq!(unsafe { libc::write(fd, c"x".as_ptr().cast(), 1) }, 1);
    close(fd);
    path
}

fn watch(directory: &CStr) -> (libc::c_int, libc::c_int) {
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    assert!(fd >= 0, "inotify_init1 failed: {}", errno());
    let mask = libc::IN_CREATE | libc::IN_MODIFY | libc::IN_CLOSE_WRITE;
    let watch = unsafe { libc::inotify_add_watch(fd, directory.as_ptr(), mask) };
    assert!(watch >= 0, "inotify_add_watch failed: {}", errno());
    (fd, watch)
}

fn read_inotify(fd: libc::c_int) -> Vec<(u32, String)> {
    let mut buffer = [0_u8; 4096];
    let bytes = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    assert!(bytes > 0, "inotify read failed: {}", errno());
    let mut events = Vec::new();
    let mut offset = 0;
    while offset < bytes as usize {
        let event = unsafe {
            ptr::read_unaligned(buffer.as_ptr().add(offset).cast::<libc::inotify_event>())
        };
        let name = if event.len == 0 {
            String::new()
        } else {
            let name = unsafe {
                CStr::from_ptr(
                    buffer
                        .as_ptr()
                        .add(offset + size_of::<libc::inotify_event>())
                        .cast(),
                )
            };
            name.to_string_lossy().into_owned()
        };
        events.push((event.mask, name));
        offset += size_of::<libc::inotify_event>() + event.len as usize;
    }
    events
}

fn clean_directory(directory: &CStr, files: &[CString]) {
    for file in files {
        assert_eq!(unsafe { libc::unlink(file.as_ptr()) }, 0);
    }
    assert_eq!(unsafe { libc::rmdir(directory.as_ptr()) }, 0);
}

fn inotify_guest() {
    let (directory, mut files) = test_directory();
    let (fd, watch) = watch(&directory);
    files.push(create_file(&directory, "first"));
    files.push(create_file(&directory, "second"));

    let events = read_inotify(fd);
    assert_eq!(events.len(), 6);
    for (mask, name) in events {
        println!("{mask:08x}:{name}");
    }

    assert_eq!(unsafe { libc::inotify_rm_watch(fd, watch) }, 0);
    close(fd);
    clean_directory(&directory, &files);
}

fn epoll_add(epfd: libc::c_int, fd: libc::c_int, tag: u64) {
    let mut event = libc::epoll_event {
        events: libc::EPOLLIN as u32,
        u64: tag,
    };
    assert_eq!(
        unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut event) },
        0,
        "epoll_ctl failed: {}",
        errno()
    );
}

fn mixed_epoll_guest() {
    let timer = arm_timer();
    let signal = signal_fd();
    let (directory, mut files) = test_directory();
    let (inotify, watch) = watch(&directory);
    let eventfd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    assert!(eventfd >= 0, "eventfd failed: {}", errno());
    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    assert!(epfd >= 0, "epoll_create1 failed: {}", errno());

    epoll_add(epfd, timer, 1);
    epoll_add(epfd, signal, 2);
    epoll_add(epfd, inotify, 3);
    epoll_add(epfd, eventfd, 4);

    let one = 1_u64;
    assert_eq!(
        unsafe { libc::write(eventfd, ptr::from_ref(&one).cast(), size_of::<u64>()) },
        size_of::<u64>() as isize
    );
    assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);
    files.push(create_file(&directory, "epoll"));

    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
    let tags = (0..MAX_ATTEMPTS)
        .find_map(|_| {
            let count = unsafe { libc::epoll_wait(epfd, events.as_mut_ptr(), 4, 0) };
            assert!(count >= 0, "epoll_wait failed: {}", errno());
            if count == 4 {
                Some(
                    events[..count as usize]
                        .iter()
                        .map(|event| event.u64)
                        .collect::<Vec<_>>(),
                )
            } else {
                unsafe { libc::sched_yield() };
                None
            }
        })
        .expect("all epoll sources should become ready");
    println!("epoll={tags:?}");

    close(epfd);
    close(eventfd);
    assert_eq!(unsafe { libc::inotify_rm_watch(inotify, watch) }, 0);
    close(inotify);
    close(signal);
    close(timer);
    clean_directory(&directory, &files);
}

#[derive(Clone, Copy)]
enum TimerWaitApi {
    Poll,
    Epoll,
}

fn cross_thread_virtual_timer_guest(api: TimerWaitApi) {
    let timer = unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
        )
    };
    assert!(timer >= 0, "timerfd_create failed: {}", errno());
    let epfd = match api {
        TimerWaitApi::Poll => -1,
        TimerWaitApi::Epoll => {
            let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
            assert!(epfd >= 0, "epoll_create1 failed: {}", errno());
            epoll_add(epfd, timer, 1);
            epfd
        }
    };

    let value = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
    };
    assert_eq!(
        unsafe { libc::timerfd_settime(timer, 0, &value, ptr::null_mut()) },
        0,
        "timerfd_settime failed: {}",
        errno()
    );

    // Advance virtual time past the timer deadline without waiting
    // one host second. Host-backed readiness therefore remains false when the
    // parent checks the cross-thread arming state.
    let duration = libc::timespec {
        tv_sec: 2,
        tv_nsec: 0,
    };
    assert_eq!(unsafe { libc::nanosleep(&duration, ptr::null_mut()) }, 0);

    let observed = thread::spawn(move || {
        let readiness = match api {
            TimerWaitApi::Poll => {
                let mut fds = [libc::pollfd {
                    fd: timer,
                    events: libc::POLLIN,
                    revents: 0,
                }];
                let count = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 0) };
                format!("poll={count}:{}", fds[0].revents & libc::POLLIN)
            }
            TimerWaitApi::Epoll => {
                let mut events = [libc::epoll_event { events: 0, u64: 0 }; 1];
                let count = unsafe { libc::epoll_wait(epfd, events.as_mut_ptr(), 1, 0) };
                let data = unsafe { ptr::addr_of!(events[0].u64).read_unaligned() };
                let ready = unsafe { ptr::addr_of!(events[0].events).read_unaligned() };
                format!("epoll={count}:{data}:{}", ready & libc::EPOLLIN as u32)
            }
        };
        let mut observed_at = MaybeUninit::<libc::timespec>::uninit();
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, observed_at.as_mut_ptr()) },
            0
        );
        let observed_at = unsafe { observed_at.assume_init() };
        let observed_at_ns =
            (observed_at.tv_sec as u128) * 1_000_000_000 + observed_at.tv_nsec as u128;
        format!("{readiness} time={observed_at_ns}")
    })
    .join()
    .expect("cross-thread virtual timer waiter failed");
    println!("{observed}");

    if epfd >= 0 {
        close(epfd);
    }
    close(timer);
}

fn cross_thread_poll_timer_guest() {
    cross_thread_virtual_timer_guest(TimerWaitApi::Poll);
}

fn cross_thread_epoll_timer_guest() {
    cross_thread_virtual_timer_guest(TimerWaitApi::Epoll);
}

fn errno() -> libc::c_int {
    unsafe { *libc::__errno_location() }
}

#[test]
fn timerfd_expiry_is_deterministic() {
    run_five_times(timerfd_guest);
}

#[test]
fn signalfd_delivery_is_deterministic() {
    run_five_times(signalfd_guest);
}

#[test]
fn inotify_order_is_deterministic() {
    run_five_times(inotify_guest);
}

#[test]
fn mixed_epoll_sources_are_deterministic() {
    run_five_times(mixed_epoll_guest);
}

#[test]
fn poll_timerfd_readiness_uses_virtual_time_across_threads() {
    run_five_times_with_expected_stdout_prefix(
        cross_thread_poll_timer_guest,
        Some(b"poll=1:1 time="),
    );
}

#[test]
fn epoll_timerfd_readiness_uses_virtual_time_across_threads() {
    run_five_times_with_expected_stdout_prefix(
        cross_thread_epoll_timer_guest,
        Some(b"epoll=1:1:1 time="),
    );
}
