/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! misc syscall tests

mod notification_fds;
mod vfork;

use nix::unistd;

#[global_allocator]
static ALLOC: test_allocator::Global = test_allocator::Global;

#[derive(Clone, Copy)]
struct HardwareRandomFeatures {
    rdrand: bool,
    rdseed: bool,
}

fn hardware_random_features() -> HardwareRandomFeatures {
    let cpuid = raw_cpuid::CpuId::new();
    HardwareRandomFeatures {
        rdrand: cpuid.get_feature_info().is_some_and(|f| f.has_rdrand()),
        rdseed: cpuid
            .get_extended_feature_info()
            .is_some_and(|f| f.has_rdseed()),
    }
}

fn cpuid_faulting_supported() -> bool {
    const ARCH_SET_CPUID: libc::c_int = 0x1012;

    let child = unsafe { libc::fork() };
    assert!(child >= 0, "failed to fork CPUID capability probe");
    if child == 0 {
        let result = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_CPUID, 0) };
        unsafe { libc::_exit(i32::from(result != 0)) };
    }

    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

fn det_test_fn_without_pmu<F>(f: F)
where
    F: Fn(),
{
    let config = detcore::Config {
        preemption_timeout: None,
        ..Default::default()
    };
    detcore_testutils::det_test_fn_with_config(true, f, config, detcore_testutils::expect_success)
}

fn det_test_fn_sequential_without_pmu<F>(f: F)
where
    F: Fn(),
{
    det_test_fn_sequential_without_pmu_with_post_fork(detcore::RunsPostFork::Child, f);
}

fn det_test_fn_sequential_without_pmu_with_post_fork<F>(runs_post_fork: detcore::RunsPostFork, f: F)
where
    F: Fn(),
{
    let config = detcore::Config {
        preemption_timeout: None,
        sequentialize_threads: true,
        runs_post_fork,
        ..Default::default()
    };
    detcore_testutils::det_test_fn_with_config(true, f, config, detcore_testutils::expect_success)
}

fn madvise_result(address: *mut libc::c_void, len: usize, advice: libc::c_int) -> Result<(), i32> {
    let result = unsafe { libc::madvise(address, len, advice) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .expect("madvise failure must set errno"))
    }
}

fn run_madvise_policy_test(passthru_opt: bool) {
    let config = detcore::Config {
        preemption_timeout: None,
        passthru_opt,
        ..Default::default()
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        || {
            let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            assert!(page_size > 0, "sysconf(_SC_PAGESIZE) should succeed");
            let page_size = page_size as usize;
            let mapping = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    page_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(mapping, libc::MAP_FAILED, "anonymous mmap should succeed");

            let byte = mapping.cast::<u8>();
            unsafe {
                byte.write(0x5a);
            }

            assert_eq!(
                madvise_result(mapping, page_size, libc::MADV_NORMAL),
                Ok(())
            );
            assert_eq!(
                madvise_result(mapping, page_size, libc::MADV_WILLNEED),
                Ok(())
            );
            assert_eq!(
                madvise_result(unsafe { byte.add(1) }.cast(), page_size, libc::MADV_FREE),
                Err(libc::EINVAL),
                "ignored advice must still validate page alignment"
            );
            assert_eq!(
                madvise_result(unsafe { byte.add(1) }.cast(), 0, libc::MADV_FREE),
                Err(libc::EINVAL),
                "zero length does not waive address alignment"
            );
            assert_eq!(madvise_result(mapping, page_size, libc::MADV_FREE), Ok(()));
            assert_eq!(madvise_result(mapping, page_size, libc::MADV_COLD), Ok(()));
            assert_eq!(
                unsafe { byte.read() },
                0x5a,
                "ignored advice changed memory"
            );

            for advice in [
                libc::MADV_POPULATE_READ,
                libc::MADV_POPULATE_WRITE,
                libc::MADV_COLLAPSE,
            ] {
                assert_eq!(
                    madvise_result(mapping, page_size, advice),
                    Err(libc::EINVAL)
                );
                assert_eq!(
                    madvise_result(std::ptr::null_mut(), 0, advice),
                    Ok(()),
                    "known zero-length advice should succeed"
                );
            }
            for advice in [libc::MADV_HWPOISON, libc::MADV_SOFT_OFFLINE] {
                assert_eq!(madvise_result(mapping, page_size, advice), Err(libc::EPERM));
                assert_eq!(
                    madvise_result(std::ptr::null_mut(), 0, advice),
                    Ok(()),
                    "known zero-length advice should succeed"
                );
            }
            assert_eq!(
                madvise_result(
                    unsafe { byte.add(1) }.cast(),
                    page_size,
                    libc::MADV_HWPOISON
                ),
                Err(libc::EINVAL),
                "common validation precedes the fixed policy error"
            );
            assert_eq!(
                madvise_result(std::ptr::null_mut(), 0, i32::MAX),
                Err(libc::EINVAL),
                "zero length must not make unknown advice valid"
            );

            assert_eq!(
                madvise_result(mapping, page_size, libc::MADV_DONTNEED),
                Ok(())
            );
            assert_eq!(unsafe { byte.read() }, 0, "MADV_DONTNEED was not forwarded");
            assert_eq!(unsafe { libc::munmap(mapping, page_size) }, 0);

            assert_eq!(
                madvise_result(mapping, page_size, libc::MADV_FREE),
                Ok(()),
                "normalized advice does not consult backend-specific mapping state"
            );

            let shared = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    page_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(shared, libc::MAP_FAILED, "shared mmap should succeed");
            let shared_byte = shared.cast::<u8>();
            unsafe { shared_byte.write(0xa5) };
            assert_eq!(
                madvise_result(shared, page_size, libc::MADV_FREE),
                Ok(()),
                "normalized reclaim advice has a fixed no-op contract"
            );
            assert_eq!(unsafe { shared_byte.read() }, 0xa5);
            assert_eq!(unsafe { libc::munmap(shared, page_size) }, 0);
        },
        config,
        detcore_testutils::expect_success,
    );
}

#[test]
fn madvise_policy_is_deterministic_and_preserves_semantic_advice() {
    run_madvise_policy_test(false);
}

#[test]
fn passthru_opt_still_intercepts_madvise() {
    run_madvise_policy_test(true);
}

#[test]
fn sched_affinity_is_normalized_to_virtual_cpu_zero() {
    det_test_fn_sequential_without_pmu(|| {
        const VIRTUAL_CPUSET_BYTES: usize = 16;

        let mut mask = [0xaa_u8; 32];
        let result = unsafe {
            libc::syscall(
                libc::SYS_sched_getaffinity,
                0,
                mask.len(),
                mask.as_mut_ptr(),
            )
        };
        assert_eq!(result, VIRTUAL_CPUSET_BYTES as libc::c_long);
        assert_eq!(mask[0], 1);
        assert!(mask[1..VIRTUAL_CPUSET_BYTES].iter().all(|byte| *byte == 0));
        assert!(
            mask[VIRTUAL_CPUSET_BYTES..]
                .iter()
                .all(|byte| *byte == 0xaa),
            "sched_getaffinity must not overwrite bytes beyond its return value"
        );

        let mut short_mask = [0_u8; VIRTUAL_CPUSET_BYTES - 1];
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_sched_getaffinity,
                    0,
                    short_mask.len(),
                    short_mask.as_mut_ptr(),
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );

        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_sched_getaffinity,
                    0,
                    VIRTUAL_CPUSET_BYTES + 1,
                    mask.as_mut_ptr(),
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );

        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_sched_getaffinity,
                    0,
                    short_mask.len(),
                    std::ptr::null_mut::<u8>(),
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );

        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_sched_getaffinity,
                    0,
                    VIRTUAL_CPUSET_BYTES,
                    std::ptr::null_mut::<u8>(),
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EFAULT)
        );

        let requested = [1_u8 << 1];
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_sched_setaffinity,
                    0,
                    requested.len(),
                    requested.as_ptr(),
                )
            },
            0
        );

        assert_eq!(
            unsafe { libc::syscall(libc::SYS_sched_setaffinity, 0, 0, requested.as_ptr(),) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );

        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_sched_setaffinity,
                    0,
                    requested.len(),
                    std::ptr::null::<u8>(),
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EFAULT)
        );

        let empty_request = [0_u8];
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_sched_setaffinity,
                    0,
                    empty_request.len(),
                    empty_request.as_ptr(),
                )
            },
            0,
            "even an empty requested set is normalized to virtual CPU 0"
        );

        mask.fill(0xaa);
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_sched_getaffinity,
                    0,
                    mask.len(),
                    mask.as_mut_ptr(),
                )
            },
            VIRTUAL_CPUSET_BYTES as libc::c_long
        );
        assert_eq!(mask[0], 1);
        assert!(mask[1..VIRTUAL_CPUSET_BYTES].iter().all(|byte| *byte == 0));
    });
}

#[test]
fn waitid_polls_until_child_exit_and_supports_wnohang() {
    det_test_fn_sequential_without_pmu(|| {
        let mut pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);

        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork should succeed");
        if child == 0 {
            unsafe {
                libc::close(pipe[1]);
                let mut byte = 0_u8;
                let read = libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
                libc::close(pipe[0]);
                libc::_exit(if read == 1 { 42 } else { 1 });
            }
        }

        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        usage.ru_maxrss = 123;
        unsafe {
            libc::close(pipe[0]);
        }

        let mut invalid_options_info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_waitid,
                    libc::P_PID,
                    child,
                    &mut invalid_options_info,
                    0,
                    std::ptr::null_mut::<libc::rusage>(),
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );

        let mut pgrp_info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_waitid,
                    libc::P_PGID,
                    0,
                    &mut pgrp_info,
                    libc::WEXITED | libc::WNOHANG,
                    std::ptr::null_mut::<libc::rusage>(),
                )
            },
            0
        );
        assert_eq!(unsafe { pgrp_info.si_pid() }, 0);

        let pidfd =
            unsafe { libc::syscall(libc::SYS_pidfd_open, child, libc::O_NONBLOCK) } as libc::c_int;
        assert!(pidfd >= 0, "pidfd_open with O_NONBLOCK should succeed");
        let mut pidfd_info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let pidfd_wait = unsafe {
            libc::syscall(
                libc::SYS_waitid,
                libc::P_PIDFD,
                pidfd,
                &mut pidfd_info,
                libc::WEXITED,
                std::ptr::null_mut::<libc::rusage>(),
            )
        };
        assert_eq!(pidfd_wait, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN)
        );
        assert_eq!(unsafe { libc::close(pidfd) }, 0);

        let blocking_pidfd =
            unsafe { libc::syscall(libc::SYS_pidfd_open, child, 0) } as libc::c_int;
        assert!(blocking_pidfd >= 0, "blocking pidfd_open should succeed");
        let mut blocking_pidfd_info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_waitid,
                    libc::P_PIDFD,
                    blocking_pidfd,
                    &mut blocking_pidfd_info,
                    libc::WEXITED,
                    std::ptr::null_mut::<libc::rusage>(),
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EOPNOTSUPP)
        );
        assert_eq!(unsafe { libc::close(blocking_pidfd) }, 0);

        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_waitid,
                    libc::P_PID,
                    child,
                    std::ptr::null_mut::<libc::siginfo_t>(),
                    libc::WEXITED | libc::WNOHANG,
                    std::ptr::null_mut::<libc::rusage>(),
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EFAULT)
        );

        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let no_event = unsafe {
            libc::syscall(
                libc::SYS_waitid,
                libc::P_PID,
                child,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
                &mut usage,
            )
        };
        assert_eq!(no_event, 0);
        assert_eq!(unsafe { info.si_pid() }, 0);
        assert_eq!(usage.ru_maxrss, 123);

        let byte = 1_u8;
        assert_eq!(
            unsafe { libc::write(pipe[1], (&byte as *const u8).cast(), 1) },
            1
        );
        unsafe {
            libc::close(pipe[1]);
        }

        let waited = unsafe {
            libc::syscall(
                libc::SYS_waitid,
                libc::P_PID,
                child,
                &mut info,
                libc::WEXITED,
                &mut usage,
            )
        };
        assert_eq!(waited, 0);
        assert_eq!(unsafe { info.si_pid() }, child);
        assert_eq!(info.si_code, libc::CLD_EXITED);
        assert_eq!(unsafe { info.si_status() }, 42);
        assert_eq!(unsafe { info.si_utime() }, 0);
        assert_eq!(unsafe { info.si_stime() }, 0);
        assert_eq!(usage.ru_utime.tv_sec, 0);
        assert_eq!(usage.ru_utime.tv_usec, 0);
        assert_eq!(usage.ru_stime.tv_sec, 0);
        assert_eq!(usage.ru_stime.tv_usec, 0);
        assert_eq!(usage.ru_maxrss, 0);
    });
}

#[test]
fn ordinary_clone_child_starts_before_parent_resumes() {
    det_test_fn_sequential_without_pmu(|| {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        let child_started = Arc::new(AtomicBool::new(false));
        let child_flag = Arc::clone(&child_started);

        let child = std::thread::spawn(move || {
            child_flag.store(true, Ordering::SeqCst);
        });

        assert!(
            child_started.load(Ordering::SeqCst),
            "an ordinary clone child must receive its startup turn before the parent resumes"
        );

        child.join().expect("child thread should exit cleanly");
    });
}

#[test]
fn ordinary_clone_parent_mode_can_resume_before_child() {
    det_test_fn_sequential_without_pmu_with_post_fork(detcore::RunsPostFork::Parent, || {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        let child_started = Arc::new(AtomicBool::new(false));
        let child_flag = Arc::clone(&child_started);

        let child = std::thread::spawn(move || {
            child_flag.store(true, Ordering::SeqCst);
        });

        assert!(
            !child_started.load(Ordering::SeqCst),
            "parent mode must permit the parent to resume before child startup"
        );

        child.join().expect("child thread should exit cleanly");
    });
}

#[test]
fn dup_shares_status_flags_but_not_cloexec() {
    det_test_fn_sequential_without_pmu(|| {
        let mut sockets = [0; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_NONBLOCK,
                    0,
                    sockets.as_mut_ptr(),
                )
            },
            0
        );

        let duplicate = unsafe { libc::fcntl(sockets[0], libc::F_DUPFD_CLOEXEC, 0) };
        assert!(duplicate >= 0);
        assert_ne!(
            unsafe { libc::fcntl(duplicate, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        assert_eq!(
            unsafe { libc::fcntl(sockets[0], libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert_ne!(
            unsafe { libc::fcntl(duplicate, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );

        let mut byte = 0_u8;
        assert_eq!(
            unsafe { libc::read(duplicate, (&mut byte as *mut u8).cast(), 1) },
            -1
        );
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EAGAIN);

        assert_eq!(unsafe { libc::close(duplicate) }, 0);
        assert_eq!(unsafe { libc::close(sockets[0]) }, 0);
        assert_eq!(unsafe { libc::close(sockets[1]) }, 0);
    });
}

#[test]
fn bound_port_survives_closing_dup_alias() {
    det_test_fn_sequential_without_pmu(|| {
        fn bind_loopback_ephemeral(fd: libc::c_int) -> libc::c_int {
            let mut address = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: 0,
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes([127, 0, 0, 1]),
                },
                sin_zero: [0; 8],
            };
            unsafe {
                libc::bind(
                    fd,
                    (&mut address as *mut libc::sockaddr_in).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }

        let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(socket >= 0);
        let mut first_bound = false;
        for _ in 0..128 {
            if bind_loopback_ephemeral(socket) == 0 {
                first_bound = true;
                break;
            }
            assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EADDRINUSE);
        }
        assert!(first_bound, "no deterministic ephemeral port was available");

        let duplicate = unsafe { libc::dup(socket) };
        assert!(duplicate >= 0);
        assert_eq!(unsafe { libc::close(socket) }, 0);

        let second = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(second >= 0);
        assert_eq!(
            bind_loopback_ephemeral(second),
            0,
            "closing one dup alias must not free its bound port reservation"
        );

        assert_eq!(unsafe { libc::close(duplicate) }, 0);
        assert_eq!(unsafe { libc::close(second) }, 0);
    });
}

#[test]
fn shared_futex_modes_are_supported_and_validate_bitsets() {
    det_test_fn_sequential_without_pmu(|| {
        let futex = 0_u32;
        assert_eq!(
            unsafe { libc::syscall(libc::SYS_futex, &futex, libc::FUTEX_WAKE, 1) },
            0,
            "a shared-mode wake with no waiters should succeed"
        );
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    &futex,
                    libc::FUTEX_WAKE_BITSET | libc::FUTEX_PRIVATE_FLAG,
                    1,
                    std::ptr::null::<libc::timespec>(),
                    std::ptr::null::<u32>(),
                    0,
                )
            },
            -1
        );
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EINVAL);
    });
}

#[test]
fn shared_anonymous_futex_wakes_across_processes() {
    det_test_fn_sequential_without_pmu(|| {
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(mapping, libc::MAP_FAILED);
        let futex = mapping.cast::<u32>();
        unsafe { futex.write(0) };

        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork should succeed");
        if child == 0 {
            let waited = unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    futex,
                    libc::FUTEX_WAIT,
                    0,
                    std::ptr::null::<libc::timespec>(),
                    std::ptr::null::<u32>(),
                    0,
                )
            };
            unsafe { libc::_exit(i32::from(waited != 0)) };
        }

        let mut woke = 0;
        for _ in 0..1024 {
            woke = unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    futex,
                    libc::FUTEX_WAKE,
                    1,
                    std::ptr::null::<libc::timespec>(),
                    std::ptr::null::<u32>(),
                    0,
                )
            };
            if woke == 1 {
                break;
            }
            assert_eq!(unsafe { libc::sched_yield() }, 0);
        }
        assert_eq!(
            woke, 1,
            "parent should wake the child through the shared mapping"
        );

        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        assert_eq!(unsafe { libc::munmap(mapping, 4096) }, 0);
    });
}

#[test]
fn dup2_same_fd_preserves_cloexec() {
    det_test_fn_sequential_without_pmu(|| {
        let path = b"/dev/null\0";
        let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
        assert!(fd >= 0);
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );

        assert_eq!(unsafe { libc::dup2(fd, fd) }, fd);
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0,
            "dup2(fd, fd) must leave descriptor flags unchanged"
        );
        assert_eq!(unsafe { libc::close(fd) }, 0);
    });
}

#[test]
fn failed_exec_preserves_shared_fd_table() {
    det_test_fn_sequential_without_pmu(|| {
        use std::ffi::CString;
        use std::sync::Arc;
        use std::sync::atomic::AtomicI32;
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::sync_channel;

        let path = b"/dev/null\0";
        let original = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY) };
        assert!(original >= 0);

        let shared_fd = Arc::new(AtomicI32::new(-1));
        let worker_fd = Arc::clone(&shared_fd);
        let (exec_failed_tx, exec_failed_rx) = sync_channel(0);
        let (continue_tx, continue_rx) = sync_channel(0);
        let (finished_tx, finished_rx) = sync_channel(0);
        let worker = std::thread::spawn(move || {
            let missing = CString::new("/definitely/missing/hermit-exec").expect("valid path");
            let argv = [missing.as_ptr(), std::ptr::null()];
            let envp: [*const libc::c_char; 1] = [std::ptr::null()];
            assert_eq!(
                unsafe { libc::execve(missing.as_ptr(), argv.as_ptr(), envp.as_ptr()) },
                -1
            );
            assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::ENOENT);
            exec_failed_tx.send(()).expect("notify parent");
            continue_rx.recv().expect("wait for sibling mutation");

            let fd = worker_fd.load(Ordering::SeqCst);
            let mut byte = 0_u8;
            assert_eq!(
                unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) },
                0,
                "failed exec must restore the exact CLONE_FILES table"
            );
            finished_tx.send(()).expect("notify parent of completion");
        });

        exec_failed_rx.recv().expect("worker should fail exec");
        let duplicate = unsafe { libc::fcntl(original, libc::F_DUPFD, 0) };
        assert!(duplicate >= 0);
        shared_fd.store(duplicate, Ordering::SeqCst);
        continue_tx.send(()).expect("release worker");
        finished_rx
            .recv()
            .expect("worker should observe the duplicate");
        drop(worker);

        assert_eq!(unsafe { libc::close(duplicate) }, 0);
        assert_eq!(unsafe { libc::close(original) }, 0);
    });
}

#[test]
fn futex_wait_bitset_timeout_is_absolute_and_removes_waiter() {
    det_test_fn_sequential_without_pmu(|| {
        fn as_nanos(ts: libc::timespec) -> i128 {
            i128::from(ts.tv_sec) * 1_000_000_000 + i128::from(ts.tv_nsec)
        }

        let futex = 0_u32;
        let mut before = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut before) },
            0
        );
        let mut deadline = before;
        deadline.tv_nsec += 5_000_000;
        if deadline.tv_nsec >= 1_000_000_000 {
            deadline.tv_sec += 1;
            deadline.tv_nsec -= 1_000_000_000;
        }

        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    &futex,
                    libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG,
                    0,
                    &deadline,
                    std::ptr::null::<u32>(),
                    1_u32,
                )
            },
            -1
        );
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::ETIMEDOUT);

        let mut after = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut after) },
            0
        );
        let elapsed = as_nanos(after) - as_nanos(before);
        assert!(
            (5_000_000..1_000_000_000).contains(&elapsed),
            "absolute WAIT_BITSET deadline advanced virtual time by {elapsed}ns"
        );

        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    &futex,
                    libc::FUTEX_WAKE_BITSET | libc::FUTEX_PRIVATE_FLAG,
                    1,
                    std::ptr::null::<libc::timespec>(),
                    std::ptr::null::<u32>(),
                    1_u32,
                )
            },
            0,
            "timed-out waiter must not remain in the futex queue"
        );
    });
}

#[test]
fn getrandom_intercepted() {
    reverie_ptrace::ret_without_perf!();
    detcore_testutils::det_test_fn(|| {
        let mut got: u64 = 0;
        assert_eq!(
            unsafe { libc::syscall(libc::SYS_getrandom, &mut got as *const u64 as u64, 8, 0) },
            8
        );
        println!("SYS_getrandom 1st result: {}", got);

        let dev_urandom = b"/dev/urandom\0";
        let fd = unsafe { libc::open(dev_urandom[..].as_ptr() as *const _, libc::O_RDONLY, 0o644) };
        assert!(fd >= 0);

        assert_eq!(
            unsafe { libc::syscall(libc::SYS_read, fd, &mut got as *const u64 as u64, 8) },
            8
        );
        println!("/dev/urandom result: {}", got);
        assert!(unistd::close(fd).is_ok());

        let dev_random = b"/dev/random\0";
        let fd = unsafe { libc::open(dev_random[..].as_ptr() as *const _, libc::O_RDONLY, 0o644) };
        assert!(fd >= 0);

        assert_eq!(
            unsafe { libc::syscall(libc::SYS_read, fd, &mut got as *const u64 as u64, 8) },
            8
        );
        println!("/dev/random result: {}", got);
        assert!(unistd::close(fd).is_ok());
    })
}

#[test]
fn has_rdrand_without_detcore() {
    let features = hardware_random_features();
    assert!(
        features.rdrand,
        "ERROR: has_rdrand_without_detcore requires the host to expose RDRAND"
    );

    if !features.rdseed {
        eprintln!("host exposes RDRAND without RDSEED; RDSEED is not required by this host test");
    }
}

#[test]
fn rdrand_rdseed_is_masked() {
    let features = hardware_random_features();
    assert!(
        features.rdrand || features.rdseed,
        "ERROR: rdrand_rdseed_is_masked requires the host to expose RDRAND or RDSEED"
    );
    assert!(
        cpuid_faulting_supported(),
        "ERROR: rdrand_rdseed_is_masked requires host CPUID faulting support"
    );

    det_test_fn_without_pmu(|| {
        let cpuid = raw_cpuid::CpuId::new();
        let feature = cpuid
            .get_feature_info()
            .expect("virtual CPU should expose basic feature information");
        assert!(!feature.has_rdrand());

        let feature_ext = cpuid
            .get_extended_feature_info()
            .expect("virtual CPU should expose extended feature information");
        assert!(!feature_ext.has_rdseed());
    })
}

#[test]
fn network_syscalls_are_deterministic_across_five_runs() {
    let config = detcore::Config {
        sequentialize_threads: true,
        deterministic_io: true,
        preemption_timeout: None,
        ..Default::default()
    };

    detcore_testutils::det_test_fn_with_config_repetitions(
        5,
        true,
        || {
            use std::net::Ipv4Addr;
            use std::net::TcpListener;
            use std::net::TcpStream;
            use std::os::fd::AsRawFd;
            use std::os::unix::net::UnixListener;
            use std::os::unix::net::UnixStream;
            use std::sync::Arc;
            use std::sync::Barrier;

            fn send_exact(fd: libc::c_int, bytes: &[u8]) {
                assert_eq!(
                    unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), 0) },
                    bytes.len() as isize
                );
            }

            fn recv_exact(fd: libc::c_int, bytes: &mut [u8]) {
                assert_eq!(
                    unsafe { libc::recv(fd, bytes.as_mut_ptr().cast(), bytes.len(), 0) },
                    bytes.len() as isize
                );
            }

            let socket_fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
            assert_eq!(socket_fd, 3);
            assert_eq!(unsafe { libc::close(socket_fd) }, 0);
            println!("socket fd: {socket_fd}");

            let mut pair = [-1; 2];
            assert_eq!(
                unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) },
                0
            );
            send_exact(pair[0], b"pair");
            let mut pair_payload = [0; 4];
            recv_exact(pair[1], &mut pair_payload);
            println!("socketpair fds: {pair:?}; payload: {pair_payload:?}");
            assert_eq!(unsafe { libc::close(pair[0]) }, 0);
            assert_eq!(unsafe { libc::close(pair[1]) }, 0);

            let temp_dir = tempfile::tempdir().unwrap();
            let socket_path = temp_dir.path().join("network-determinism.sock");
            let unix_listener = UnixListener::bind(&socket_path).unwrap();
            let unix_listener_fd = unix_listener.as_raw_fd();
            let client_path = socket_path.clone();
            let unix_client = std::thread::spawn(move || {
                let client = UnixStream::connect(client_path).unwrap();
                let client_fd = client.as_raw_fd();
                send_exact(client_fd, b"unix");
                let mut ack = [0; 2];
                recv_exact(client_fd, &mut ack);
                (client_fd, ack)
            });
            let (unix_server, _) = unix_listener.accept().unwrap();
            let unix_accepted_fd = unix_server.as_raw_fd();
            let mut unix_payload = [0; 4];
            recv_exact(unix_accepted_fd, &mut unix_payload);
            send_exact(unix_accepted_fd, b"ok");
            let (unix_client_fd, unix_ack) = unix_client.join().unwrap();
            println!(
                "unix fds: listener={unix_listener_fd}, client={unix_client_fd}, accepted={unix_accepted_fd}; payload={unix_payload:?}; ack={unix_ack:?}"
            );
            drop(unix_server);
            drop(unix_listener);
            drop(temp_dir);

            // Stay on loopback while avoiding the address used by other networking tests that
            // may run concurrently.
            let tcp_listener = TcpListener::bind((Ipv4Addr::new(127, 0, 0, 42), 0)).unwrap();
            let tcp_listener_fd = tcp_listener.as_raw_fd();
            let tcp_addr = tcp_listener.local_addr().unwrap();
            assert_eq!(tcp_addr.port(), 32768);

            let barrier = Arc::new(Barrier::new(3));
            let clients: Vec<_> = (*b"AB")
                .into_iter()
                .map(|label| {
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        let client = TcpStream::connect(tcp_addr).unwrap();
                        let client_fd = client.as_raw_fd();
                        send_exact(client_fd, &[label]);
                        let mut ack = [0; 1];
                        recv_exact(client_fd, &mut ack);
                        (label, client_fd, ack[0])
                    })
                })
                .collect();
            barrier.wait();

            let mut accepted_fds = Vec::new();
            let mut accepted_order = Vec::new();
            let mut accepted_connections = Vec::new();
            for _ in 0..clients.len() {
                let (server, _) = tcp_listener.accept().unwrap();
                accepted_fds.push(server.as_raw_fd());
                let mut label = [0; 1];
                recv_exact(server.as_raw_fd(), &mut label);
                accepted_order.push(label[0]);
                send_exact(server.as_raw_fd(), &[label[0].to_ascii_lowercase()]);
                accepted_connections.push(server);
            }
            let client_results: Vec<_> = clients
                .into_iter()
                .map(|client| client.join().unwrap())
                .collect();
            assert_eq!(
                client_results
                    .iter()
                    .map(|(label, _, ack)| (*label, *ack))
                    .collect::<Vec<_>>(),
                vec![(b'A', b'a'), (b'B', b'b')]
            );
            println!(
                "tcp listener: fd={tcp_listener_fd}, addr={tcp_addr}; accepted_fds={accepted_fds:?}; order={accepted_order:?}; clients={client_results:?}"
            );
        },
        config,
        detcore_testutils::expect_success,
    );
}
