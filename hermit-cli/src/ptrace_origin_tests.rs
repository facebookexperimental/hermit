/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in LICENSE.
 */

use std::os::fd::RawFd;
use std::task::Waker;

use super::*;

struct InertCanary {
    control: Rc<Control>,
    polls: Rc<Cell<usize>>,
}
impl Future for InertCanary {
    type Output = Result<u32, Error>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let n = self.polls.get() + 1;
        self.polls.set(n);
        if n == 1 {
            self.control.checkpoint.set(true);
            Poll::Pending
        } else {
            Poll::Ready(Ok(97))
        }
    }
}
fn inert_owner() -> (HermitCleanupUnconfirmed, Rc<Entry>, Rc<Cell<usize>>) {
    let polls = Rc::new(Cell::new(0));
    let diagnostic = super::pending(run(None, |control| InertCanary {
        control,
        polls: polls.clone(),
    }));
    let entry = OWNERS.with(|owners| owners.borrow().get(&diagnostic.key).unwrap().clone());
    assert_eq!(polls.get(), 1);
    (diagnostic, entry, polls)
}
fn deadline() -> Instant {
    let absolute: u64 = std::env::var("HERMIT_OWNER_TEST_DEADLINE")
        .unwrap()
        .parse()
        .unwrap();
    let remaining = absolute
        .checked_sub(super::monotonic_ns())
        .expect("original deadline elapsed");
    Instant::now() + Duration::from_nanos(remaining)
}
fn wait_child(pid: i32, deadline: Instant) -> i32 {
    loop {
        let mut status = 0;
        let got = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        assert!(
            got == 0 || got == pid,
            "waitpid: {}",
            std::io::Error::last_os_error()
        );
        if got == pid {
            assert!(Instant::now() < deadline);
            return status;
        }
        assert!(Instant::now() < deadline, "original wait deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn pipe() -> [i32; 2] {
    let mut descriptors = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    descriptors
}
fn write_byte(fd: RawFd, byte: u8) {
    assert_eq!(
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) },
        1
    );
}
fn read_byte(fd: RawFd, deadline: Instant) -> u8 {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("original pipe deadline");
        let millis = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut p, 1, millis) };
        if ready == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        assert_eq!(ready, 1, "pipe readiness within original deadline");
        assert!(Instant::now() < deadline);
        let mut b = 0;
        assert_eq!(unsafe { libc::read(fd, (&mut b as *mut u8).cast(), 1) }, 1);
        return b;
    }
}
fn exit(status: i32) -> ! {
    unsafe { libc::_exit(status) }
}
fn fork() -> i32 {
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
    pid
}
fn namespace_case(body: impl FnOnce(Instant) + Copy) {
    let bound = deadline();
    // Outer libtest is threaded. The existing Container clone is containment,
    // not a universal fork-safety proof. All operation creation and subsequent
    // forks happen inside its observed single-thread namespace child.
    reverie::process::Container::new()
        .unshare(reverie::process::Namespace::USER | reverie::process::Namespace::PID)
        .run(|| {
            assert_eq!(unsafe { libc::getpid() }, 1);
            assert_eq!(std::fs::read_dir("/proc/self/task").unwrap().count(), 1);
            body(bound);
            assert!(Instant::now() < bound);
        })
        .unwrap();
}

#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}
#[repr(C)]
struct Shared {
    descendant: std::sync::atomic::AtomicI32,
    origin_reaped: std::sync::atomic::AtomicBool,
}

#[test]
fn actual_pid_reuse_refuses_copied_owner_before_poll() {
    super::isolated(
        "origin_tests::actual_pid_reuse_refuses_copied_owner_before_poll",
        || {
            namespace_case(|bound| {
                let mapping = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        std::mem::size_of::<Shared>(),
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                assert_ne!(mapping, libc::MAP_FAILED);
                unsafe {
                    mapping.cast::<Shared>().write(Shared {
                        descendant: std::sync::atomic::AtomicI32::new(0),
                        origin_reaped: std::sync::atomic::AtomicBool::new(false),
                    });
                }
                let shared = unsafe { &*mapping.cast::<Shared>() };
                let ack = pipe();
                let live = pipe();
                let p = fork();
                if p == 0 {
                    let (diagnostic, entry, polls) = inert_owner();
                    let original_pid = unsafe { libc::getpid() };
                    let c = fork();
                    if c == 0 {
                        assert!(matches!(
                            entry.check_origin(),
                            Err(RecoveryRefusal::WrongIdentity)
                        ));
                        assert_eq!(polls.get(), 1);
                        write_byte(live[1], 1);
                        assert_eq!(read_byte(ack[0], bound), 2);
                        assert!(shared.origin_reaped.load(Ordering::Acquire));
                        let requested = original_pid as u32;
                        let arguments = CloneArgs {
                            exit_signal: libc::SIGCHLD as u64,
                            set_tid: (&requested as *const u32) as u64,
                            set_tid_size: 1,
                            ..Default::default()
                        };
                        // Exactly one controlled allocation; a refusal is not a skip.
                        let g = unsafe {
                            libc::syscall(
                                libc::SYS_clone3,
                                &arguments,
                                std::mem::size_of::<CloneArgs>(),
                            )
                        };
                        assert!(
                            g >= 0,
                            "one clone3 set_tid allocation refused: {}",
                            std::io::Error::last_os_error()
                        );
                        if g == 0 {
                            assert_eq!(unsafe { libc::getpid() }, original_pid);
                            diagnostic.origin.check().unwrap(); // all former three checks pass
                            assert!(matches!(
                                entry.original_process.check(),
                                Err(RecoveryRefusal::OriginalProcessUnavailable { .. })
                            ));
                            let eligibility = entry.check_origin();
                            if eligibility.is_ok() {
                                // Deliberately inert registered operation only. Never
                                // poll the copied Tokio Runtime in the mutant. This is
                                // an eligibility discriminator, not arbitrary-fork safety.
                                let mut stored = entry.driver.borrow_mut();
                                let waker = Waker::noop();
                                let mut cx = Context::from_waker(waker);
                                let _ = stored.future.as_mut().poll(&mut cx);
                            }
                            eprintln!(
                                "ORIGIN_REUSE pid={} triple_match=true old_pidfd_ready=true eligibility={eligibility:?} canary_polls={} origin_reaped=true",
                                original_pid,
                                polls.get()
                            );
                            assert_eq!(polls.get(), 1, "copied owner was eligible for polling");
                            assert!(matches!(
                                eligibility,
                                Err(RecoveryRefusal::OriginalProcessUnavailable { .. })
                            ));
                            super::refusal(diagnostic.resume::<u32>(), |e| {
                                matches!(e, RecoveryRefusal::OriginalProcessUnavailable { .. })
                            });
                            assert_eq!(polls.get(), 1);
                            assert!(
                                OWNERS.with(|owners| owners.borrow().contains_key(&diagnostic.key))
                            );
                            assert_eq!(*UNRESOLVED.lock().unwrap(), 1);
                            exit(0);
                        }
                        assert_eq!(g, original_pid as i64);
                        assert_eq!(wait_child(g as i32, bound), 0);
                        exit(0);
                    }
                    shared.descendant.store(c, Ordering::Release);
                    assert_eq!(read_byte(live[0], bound), 1);
                    exit(41);
                }
                // PID1 survives. Actual original-parent wait makes old P reusable.
                assert_eq!(wait_child(p, bound), 41 << 8);
                shared.origin_reaped.store(true, Ordering::Release);
                let c = shared.descendant.load(Ordering::Acquire);
                assert!(c > 1 && c != p);
                write_byte(ack[1], 2);
                assert_eq!(wait_child(c, bound), 0);
                for fd in ack.into_iter().chain(live) {
                    unsafe {
                        libc::close(fd);
                    }
                }
                assert_eq!(
                    unsafe { libc::munmap(mapping, std::mem::size_of::<Shared>()) },
                    0
                );
            })
        },
    );
}

#[test]
fn nested_pid1_refuses_and_original_namespace_recovers() {
    super::isolated(
        "origin_tests::nested_pid1_refuses_and_original_namespace_recovers",
        || {
            namespace_case(|bound| {
                let (diagnostic, entry, polls) = inert_owner();
                assert_eq!(diagnostic.origin.pid, 1);
                assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWPID) }, 0);
                let child = fork();
                if child == 0 {
                    assert_eq!(unsafe { libc::getpid() }, 1);
                    assert_eq!(std::thread::current().id(), diagnostic.origin.thread);
                    assert!(matches!(
                        entry.check_origin(),
                        Err(RecoveryRefusal::WrongIdentity)
                    ));
                    super::refusal(diagnostic.resume::<u32>(), |e| {
                        matches!(e, RecoveryRefusal::WrongIdentity)
                    });
                    assert_eq!(polls.get(), 1);
                    exit(0);
                }
                assert_eq!(wait_child(child, bound), 0);
                // unshare changed pid_for_children, not this owner's active namespace.
                assert_eq!(diagnostic.resume::<u32>().unwrap(), 97);
                assert_eq!(polls.get(), 2);
            })
        },
    );
}

#[test]
fn owner_thread_recovers_after_process_leader_exits() {
    super::isolated(
        "origin_tests::owner_thread_recovers_after_process_leader_exits",
        || {
            namespace_case(|bound| {
                let witness = pipe();
                let child = fork();
                if child == 0 {
                    let (send, receive) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let (diagnostic, entry, polls) = inert_owner();
                        send.send(()).unwrap();
                        loop {
                            let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
                            let state = stat
                                .rsplit_once(')')
                                .unwrap()
                                .1
                                .split_whitespace()
                                .next()
                                .unwrap();
                            if state == "Z" {
                                break;
                            }
                            assert!(
                                Instant::now() < bound,
                                "leader did not reach actual zombie state"
                            );
                            std::thread::yield_now();
                        }
                        entry.original_process.check().unwrap();
                        assert_eq!(diagnostic.resume::<u32>().unwrap(), 97);
                        assert_eq!(polls.get(), 2);
                        write_byte(witness[1], 23);
                        exit(23);
                    });
                    receive.recv().unwrap();
                    unsafe {
                        libc::syscall(libc::SYS_exit, 7);
                    }
                    unreachable!();
                }
                assert_eq!(wait_child(child, bound), 23 << 8);
                assert_eq!(read_byte(witness[0], bound), 23);
                for fd in witness {
                    unsafe {
                        libc::close(fd);
                    }
                }
            })
        },
    );
}
