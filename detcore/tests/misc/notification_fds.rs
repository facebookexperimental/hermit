use std::ffi::CStr;
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::ptr;

use detcore::Config;
use detcore::Detcore;
use reverie::ExitStatus;

const RUNS: usize = 5;
const MAX_ATTEMPTS: usize = 100_000;

fn run_five_times(guest: fn()) {
    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        ..Default::default()
    };
    let mut expected = None;

    for run in 1..=RUNS {
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

        if let Some(expected) = &expected {
            assert_eq!(
                &output.stdout, expected,
                "notification output diverged on run {run}"
            );
        } else {
            expected = Some(output.stdout);
        }
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
