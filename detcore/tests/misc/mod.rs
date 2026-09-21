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

use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;

use nix::unistd;
use reverie::Error;
use reverie::ExitStatus;
use reverie::Guest;
use reverie::Subscription;
use reverie::Tool;
use reverie::syscalls::Syscall;

#[global_allocator]
static ALLOC: test_allocator::Global = test_allocator::Global;

/// Test-only inner tool that turns an otherwise inert getter into a raw kernel
/// timer-slack observation. Detcore handles the virtual getter itself, so the
/// bracket needs this lower layer to observe the physical tracee value.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
struct PhysicalTimerSlackProbe;

#[reverie::tool]
impl Tool for PhysicalTimerSlackProbe {
    type GlobalState = detcore::GlobalState;
    type ThreadState = ();

    fn subscriptions(_config: &detcore::Config) -> Subscription {
        Subscription::none()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let call = match call {
            Syscall::Prctl(call) if call.option() == libc::PR_GET_DUMPABLE => {
                Syscall::Prctl(call.with_option(libc::PR_GET_TIMERSLACK))
            }
            call => call,
        };
        Ok(guest.inject(call).await?)
    }
}

#[repr(C)]
struct TimerSlackBracketState {
    stage: AtomicU8,
    physical_before: AtomicI32,
    physical_after: AtomicI32,
}

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
    F: Fn() + Send + Sync,
{
    let config = detcore::Config {
        max_timeslice: None,
        ..Default::default()
    };
    detcore_testutils::det_test_fn_with_config(true, f, config, detcore_testutils::expect_success)
}

fn det_test_fn_sequential_without_pmu<F>(f: F)
where
    F: Fn() + Send + Sync,
{
    det_test_fn_sequential_without_pmu_with_post_fork(detcore::RunsPostFork::Child, f);
}

fn det_test_fn_sequential_without_pmu_with_post_fork<F>(runs_post_fork: detcore::RunsPostFork, f: F)
where
    F: Fn() + Send + Sync,
{
    let config = detcore::Config {
        max_timeslice: None,
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
        max_timeslice: None,
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
fn prctl_keepcaps_round_trips_deterministically() {
    // `setpriv` (used by the `date` privilege-drop wrapper) sets and reads the
    // per-thread PR_SET_KEEPCAPS flag during startup. Detcore must support it
    // instead of returning ENOSYS, which made setpriv abort with
    // "keep process capabilities failed: Function not implemented". The flag is
    // process-local, so the set/get round trip is deterministic regardless of
    // the initial state.
    det_test_fn_sequential_without_pmu(|| unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_KEEPCAPS, 1), 0);
        assert_eq!(libc::prctl(libc::PR_GET_KEEPCAPS), 1);
        assert_eq!(libc::prctl(libc::PR_SET_KEEPCAPS, 0), 0);
        assert_eq!(libc::prctl(libc::PR_GET_KEEPCAPS), 0);
    });
}

#[test]
fn timer_slack_prctl_and_procfs_share_virtual_state() {
    const DEFAULT_TIMER_SLACK_NS: libc::c_int = 50_000;
    det_test_fn_sequential_without_pmu(|| unsafe {
        assert_eq!(libc::prctl(libc::PR_GET_TIMERSLACK), DEFAULT_TIMER_SLACK_NS);

        // Detcore exposes one guest scheduling policy (SCHED_OTHER). Even a
        // successful request for an RT policy therefore cannot reach Linux's
        // physical RT/DL special case that forces timer slack to zero.
        let param = libc::sched_param { sched_priority: 1 };
        assert_eq!(libc::sched_setscheduler(0, libc::SCHED_FIFO, &param), 0);
        assert_eq!(libc::sched_getscheduler(0), libc::SCHED_OTHER);

        const REQUESTED_SLACK_NS: libc::c_int = 1_000_000;
        assert_eq!(libc::prctl(libc::PR_SET_TIMERSLACK, REQUESTED_SLACK_NS), 0);
        assert_eq!(libc::prctl(libc::PR_GET_TIMERSLACK), REQUESTED_SLACK_NS);
        assert_eq!(
            std::fs::read_to_string("/proc/self/timerslack_ns")
                .unwrap()
                .trim(),
            REQUESTED_SLACK_NS.to_string()
        );

        let tid = libc::syscall(libc::SYS_gettid) as libc::pid_t;
        assert_eq!(
            std::fs::read_to_string(format!("/proc/{tid}/timerslack_ns"))
                .unwrap()
                .trim(),
            REQUESTED_SLACK_NS.to_string()
        );
        for absent in [
            "/proc/thread-self/timerslack_ns".to_owned(),
            format!("/proc/self/task/{tid}/timerslack_ns"),
            format!("/proc/{tid}/task/{tid}/timerslack_ns"),
        ] {
            assert_eq!(
                std::fs::File::open(absent).unwrap_err().raw_os_error(),
                Some(libc::ENOENT)
            );
        }

        let read_only = std::fs::File::open("/proc/self/timerslack_ns").unwrap();
        assert_eq!(
            libc::write(read_only.as_raw_fd(), b"1".as_ptr().cast(), 1),
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
        let write_only = std::fs::OpenOptions::new()
            .write(true)
            .open("/proc/self/timerslack_ns")
            .unwrap();
        let mut denied = [0_u8; 1];
        assert_eq!(
            libc::read(
                write_only.as_raw_fd(),
                denied.as_mut_ptr().cast(),
                denied.len(),
            ),
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );

        let mut writable = std::fs::OpenOptions::new()
            .write(true)
            .open("/proc/self/timerslack_ns")
            .unwrap();
        writable.write_all(b"222222\n").unwrap();
        assert_eq!(libc::prctl(libc::PR_GET_TIMERSLACK), 222_222);

        assert_eq!(libc::prctl(libc::PR_SET_TIMERSLACK, 333_333), 0);
        let mut readable = std::fs::File::open("/proc/self/timerslack_ns").unwrap();
        assert_eq!(
            libc::read(readable.as_raw_fd(), std::ptr::null_mut(), 1),
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EFAULT)
        );
        assert_eq!(
            libc::lseek(readable.as_raw_fd(), 0, libc::SEEK_CUR),
            0,
            "a failed copy must not advance the procfs cursor"
        );

        let mut prefix = [0_u8; 2];
        readable.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"33");
        assert_eq!(libc::prctl(libc::PR_SET_TIMERSLACK, 444_444), 0);
        let mut suffix = String::new();
        readable.read_to_string(&mut suffix).unwrap();
        assert_eq!(suffix, "3333\n", "partial reads retain one snapshot");
        readable.seek(SeekFrom::Start(0)).unwrap();
        let mut rewound = String::new();
        readable.read_to_string(&mut rewound).unwrap();
        assert_eq!(rewound, "444444\n", "rewind regenerates the snapshot");

        assert_eq!(libc::prctl(libc::PR_SET_TIMERSLACK, 555_555), 0);
        let mut positioned = [0_u8; 6];
        assert_eq!(
            libc::pread(
                readable.as_raw_fd(),
                positioned.as_mut_ptr().cast(),
                positioned.len(),
                1,
            ),
            positioned.len() as isize
        );
        assert_eq!(&positioned, b"55555\n");

        let mut writable = std::fs::OpenOptions::new()
            .write(true)
            .open("/proc/self/timerslack_ns")
            .unwrap();
        writable.write_all(b"0\n").unwrap();
        assert_eq!(
            libc::prctl(libc::PR_GET_TIMERSLACK),
            DEFAULT_TIMER_SLACK_NS,
            "zero restores the thread's inherited default"
        );
    });
}

#[test]
fn timer_slack_procfs_vector_io_matches_linux() {
    det_test_fn_sequential_without_pmu(|| unsafe {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/proc/self/timerslack_ns")
            .unwrap();
        let fd = file.as_raw_fd();

        let first = b"343";
        let second = b"434\n";
        let writes = [
            libc::iovec {
                iov_base: first.as_ptr().cast_mut().cast(),
                iov_len: first.len(),
            },
            libc::iovec {
                iov_base: second.as_ptr().cast_mut().cast(),
                iov_len: second.len(),
            },
        ];
        assert_eq!(libc::writev(fd, writes.as_ptr(), writes.len() as i32), 7);
        assert_eq!(libc::prctl(libc::PR_GET_TIMERSLACK), 434);

        assert_eq!(file.seek(SeekFrom::Start(0)).unwrap(), 0);
        let mut left = [0_u8; 2];
        let mut right = [0_u8; 8];
        let reads = [
            libc::iovec {
                iov_base: left.as_mut_ptr().cast(),
                iov_len: left.len(),
            },
            libc::iovec {
                iov_base: right.as_mut_ptr().cast(),
                iov_len: right.len(),
            },
        ];
        assert_eq!(libc::readv(fd, reads.as_ptr(), reads.len() as i32), 4);
        assert_eq!(&left, b"43");
        assert_eq!(&right[..2], b"4\n");

        assert_eq!(file.seek(SeekFrom::Start(0)).unwrap(), 0);
        assert_eq!(libc::prctl(libc::PR_SET_TIMERSLACK, 246_810), 0);
        let mut untouched = [0_u8; 2];
        let bad_first = [
            libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 1,
            },
            libc::iovec {
                iov_base: untouched.as_mut_ptr().cast(),
                iov_len: untouched.len(),
            },
        ];
        assert_eq!(
            libc::readv(fd, bad_first.as_ptr(), bad_first.len() as i32),
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EFAULT)
        );
        assert_eq!(
            libc::lseek(fd, 0, libc::SEEK_CUR),
            0,
            "a failed first iovec must not advance the procfs cursor"
        );

        assert_eq!(libc::prctl(libc::PR_SET_TIMERSLACK, 987_654), 0);
        let mut partial = [0_u8; 2];
        let bad_second = [
            libc::iovec {
                iov_base: partial.as_mut_ptr().cast(),
                iov_len: partial.len(),
            },
            libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 1,
            },
        ];
        assert_eq!(
            libc::readv(fd, bad_second.as_ptr(), bad_second.len() as i32),
            partial.len() as isize
        );
        assert_eq!(&partial, b"98");
        assert_eq!(libc::lseek(fd, 0, libc::SEEK_CUR), 2);
        assert_eq!(libc::prctl(libc::PR_SET_TIMERSLACK, 111_111), 0);
        let mut retained = String::new();
        file.read_to_string(&mut retained).unwrap();
        assert_eq!(
            retained, "7654\n",
            "a later failed iovec retains only the successfully copied prefix"
        );

        let pfirst = b"515";
        let psecond = b"151\n";
        let pwrites = [
            libc::iovec {
                iov_base: pfirst.as_ptr().cast_mut().cast(),
                iov_len: pfirst.len(),
            },
            libc::iovec {
                iov_base: psecond.as_ptr().cast_mut().cast(),
                iov_len: psecond.len(),
            },
        ];
        assert_eq!(
            libc::pwritev(fd, pwrites.as_ptr(), pwrites.len() as i32, -1),
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );
        assert_eq!(
            libc::syscall(
                libc::SYS_pwritev2,
                fd,
                pwrites.as_ptr(),
                pwrites.len(),
                u64::MAX,
                0_u64,
                libc::RWF_HIPRI,
            ),
            7
        );
        assert_eq!(libc::prctl(libc::PR_GET_TIMERSLACK), 151);
        assert_eq!(
            libc::pwritev(fd, pwrites.as_ptr(), pwrites.len() as i32, 0),
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESPIPE)
        );
        assert_eq!(libc::pwrite(fd, first.as_ptr().cast(), first.len(), -1), -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );

        assert_eq!(libc::prctl(libc::PR_SET_TIMERSLACK, 987_654), 0);
        let mut pleft = [0_u8; 2];
        let mut pright = [0_u8; 4];
        let preads = [
            libc::iovec {
                iov_base: pleft.as_mut_ptr().cast(),
                iov_len: pleft.len(),
            },
            libc::iovec {
                iov_base: pright.as_mut_ptr().cast(),
                iov_len: pright.len(),
            },
        ];
        assert_eq!(
            libc::preadv(fd, preads.as_ptr(), preads.len() as i32, -1),
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );
        assert_eq!(file.seek(SeekFrom::Start(0)).unwrap(), 0);
        assert_eq!(
            libc::syscall(
                libc::SYS_preadv2,
                fd,
                preads.as_ptr(),
                preads.len(),
                u64::MAX,
                0_u64,
                libc::RWF_HIPRI,
            ),
            6
        );
        assert_eq!(&pleft, b"98");
        assert_eq!(&pright, b"7654");
        pleft.fill(0);
        pright.fill(0);
        assert_eq!(libc::preadv(fd, preads.as_ptr(), preads.len() as i32, 1), 6);
        assert_eq!(&pleft, b"87");
        assert_eq!(&pright, b"654\n");
    });
}

#[test]
fn timer_slack_procfs_binds_target_at_open() {
    det_test_fn_sequential_without_pmu(|| unsafe {
        const PARENT_SLACK_NS: libc::c_int = 1_000_000;
        assert_eq!(libc::prctl(libc::PR_SET_TIMERSLACK, PARENT_SLACK_NS), 0);

        let (tid_send, tid_recv) = std::sync::mpsc::channel();
        let (file_send, file_recv) = std::sync::mpsc::channel::<std::fs::File>();
        let (back_send, back_recv) = std::sync::mpsc::channel::<std::fs::File>();
        let worker = std::thread::spawn(move || {
            let tid = libc::syscall(libc::SYS_gettid) as libc::pid_t;
            tid_send.send(tid).unwrap();
            let mut file = file_recv.recv().unwrap();

            assert_eq!(libc::prctl(libc::PR_GET_TIMERSLACK), PARENT_SLACK_NS);
            assert_eq!(libc::prctl(libc::PR_SET_TIMERSLACK, 0), 0);
            assert_eq!(
                libc::prctl(libc::PR_GET_TIMERSLACK),
                PARENT_SLACK_NS,
                "a new thread resets to its inherited current value"
            );
            assert_eq!(
                std::fs::read_to_string("/proc/self/timerslack_ns")
                    .unwrap_err()
                    .raw_os_error(),
                Some(libc::EPERM),
                "/proc/self remains bound to the process leader"
            );

            file.seek(SeekFrom::Start(0)).unwrap();
            let mut inherited = String::new();
            file.read_to_string(&mut inherited).unwrap();
            assert_eq!(inherited.trim(), PARENT_SLACK_NS.to_string());
            let mut own = std::fs::OpenOptions::new()
                .write(true)
                .open(format!("/proc/{tid}/timerslack_ns"))
                .unwrap();
            own.write_all(b"777777\n").unwrap();
            assert_eq!(libc::prctl(libc::PR_GET_TIMERSLACK), 777_777);
            file.seek(SeekFrom::Start(0)).unwrap();
            back_send.send(file).unwrap();
        });

        let tid = tid_recv.recv().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/proc/{tid}/timerslack_ns"))
            .unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(libc::read(file.as_raw_fd(), byte.as_mut_ptr().cast(), 0), 0);
        assert_eq!(
            libc::pread(file.as_raw_fd(), byte.as_mut_ptr().cast(), 0, 0),
            0
        );
        assert_eq!(libc::lseek(file.as_raw_fd(), 0, libc::SEEK_SET), 0);
        assert_eq!(libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR), 0);
        assert_eq!(libc::lseek(file.as_raw_fd(), 0, libc::SEEK_END), -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );
        assert_eq!(
            file.read(&mut byte).unwrap_err().raw_os_error(),
            Some(libc::EPERM),
            "a live other task requires CAP_SYS_NICE"
        );
        file_send.send(file).unwrap();
        let mut file = back_recv.recv().unwrap();
        worker.join().unwrap();
        assert_eq!(libc::read(file.as_raw_fd(), byte.as_mut_ptr().cast(), 0), 0);
        assert_eq!(
            libc::pread(file.as_raw_fd(), byte.as_mut_ptr().cast(), 0, 0),
            0
        );
        assert_eq!(libc::lseek(file.as_raw_fd(), 0, libc::SEEK_SET), 0);
        assert_eq!(libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR), 0);
        assert_eq!(libc::lseek(file.as_raw_fd(), 0, libc::SEEK_END), -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );
        assert_eq!(
            file.read(&mut byte).unwrap_err().raw_os_error(),
            Some(libc::ESRCH),
            "the open description retains the exited task incarnation"
        );
        assert_eq!(
            libc::prctl(libc::PR_GET_TIMERSLACK),
            PARENT_SLACK_NS,
            "the worker's write must not disturb the leader"
        );
    });
}

#[test]
fn timer_slack_is_mediated_under_passthru_opt() {
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        passthru_opt: true,
        ..Default::default()
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        || unsafe {
            assert_eq!(libc::prctl(libc::PR_GET_TIMERSLACK), 50_000);
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/proc/self/timerslack_ns")
                .unwrap();
            file.write_all(b"123456\n").unwrap();
            assert_eq!(libc::prctl(libc::PR_GET_TIMERSLACK), 123_456);
            file.seek(SeekFrom::Start(0)).unwrap();
            let mut value = String::new();
            file.read_to_string(&mut value).unwrap();
            assert_eq!(value, "123456\n");
        },
        config,
        detcore_testutils::expect_success,
    );
}

#[test]
fn timer_slack_virtual_state_is_isolated_from_physical_tracee() {
    const PHYSICAL_SENTINEL_NS: libc::c_int = 7_654_321;
    const VIRTUAL_REQUEST_NS: libc::c_int = 1_000_000_000;
    const VIRTUAL_PROC_REQUEST_NS: libc::c_int = 888_888_888;

    struct RestoreTimerSlack(libc::c_int);
    impl Drop for RestoreTimerSlack {
        fn drop(&mut self) {
            assert_eq!(unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, self.0) }, 0);
        }
    }

    let original = unsafe { libc::prctl(libc::PR_GET_TIMERSLACK) };
    assert!(original >= 0);
    let _restore = RestoreTimerSlack(original);
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, PHYSICAL_SENTINEL_NS) },
        0
    );

    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            std::mem::size_of::<TimerSlackBracketState>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(mapping, libc::MAP_FAILED);
    let bracket = mapping.cast::<TimerSlackBracketState>();
    unsafe {
        bracket.write(TimerSlackBracketState {
            stage: AtomicU8::new(0),
            physical_before: AtomicI32::new(-1),
            physical_after: AtomicI32::new(-1),
        })
    };

    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        ..Default::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let tracer =
            reverie_ptrace::spawn_fn_with_config::<detcore::Detcore<PhysicalTimerSlackProbe>, _>(
                move || unsafe {
                    let bracket = &*bracket;
                    assert_eq!(libc::prctl(libc::PR_GET_TIMERSLACK), 50_000);
                    assert_eq!(
                        std::fs::read_to_string("/proc/self/timerslack_ns")
                            .unwrap()
                            .trim(),
                        "50000"
                    );
                    bracket
                        .physical_before
                        .store(libc::prctl(libc::PR_GET_DUMPABLE), Ordering::Release);
                    bracket.stage.store(1, Ordering::Release);
                    while bracket.stage.load(Ordering::Acquire) != 2 {
                        std::hint::spin_loop();
                    }

                    assert_eq!(libc::prctl(libc::PR_SET_TIMERSLACK, VIRTUAL_REQUEST_NS), 0);
                    assert_eq!(libc::prctl(libc::PR_GET_TIMERSLACK), VIRTUAL_REQUEST_NS);
                    assert_eq!(
                        std::fs::read_to_string("/proc/self/timerslack_ns")
                            .unwrap()
                            .trim(),
                        VIRTUAL_REQUEST_NS.to_string()
                    );
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open("/proc/self/timerslack_ns")
                        .unwrap()
                        .write_all(b"888888888\n")
                        .unwrap();
                    assert_eq!(
                        libc::prctl(libc::PR_GET_TIMERSLACK),
                        VIRTUAL_PROC_REQUEST_NS
                    );
                    bracket
                        .physical_after
                        .store(libc::prctl(libc::PR_GET_DUMPABLE), Ordering::Release);
                    bracket.stage.store(3, Ordering::Release);
                    while bracket.stage.load(Ordering::Acquire) != 4 {
                        std::hint::spin_loop();
                    }
                },
                config,
                true,
            )
            .await
            .unwrap();
        let bracket = unsafe { &*bracket };

        async fn await_stage(stage: &AtomicU8, expected: u8) {
            tokio::time::timeout(Duration::from_secs(10), async {
                while stage.load(Ordering::Acquire) != expected {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("tracee did not reach stage {expected}"));
        }

        let controller = async {
            await_stage(&bracket.stage, 1).await;
            assert_eq!(
                bracket.physical_before.load(Ordering::Acquire),
                PHYSICAL_SENTINEL_NS,
                "the launcher's physical timer slack must not seed virtual state"
            );
            bracket.stage.store(2, Ordering::Release);

            await_stage(&bracket.stage, 3).await;
            assert_eq!(
                bracket.physical_after.load(Ordering::Acquire),
                PHYSICAL_SENTINEL_NS,
                "a virtual timer-slack update must not mutate the physical tracee"
            );
            bracket.stage.store(4, Ordering::Release);
        };
        let ((), trace_result) = tokio::join!(controller, tracer.wait_with_output());
        let (output, _) = trace_result.unwrap();
        assert_eq!(output.status, ExitStatus::Exited(0));
    });

    assert_eq!(
        unsafe { libc::munmap(mapping, std::mem::size_of::<TimerSlackBracketState>()) },
        0
    );
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
fn fcntl_advisory_set_lock_succeeds() {
    det_test_fn_sequential_without_pmu(|| {
        let path = b"/tmp/detcore-fcntl-lock\0";
        let fd = unsafe {
            libc::open(
                path.as_ptr().cast(),
                libc::O_CREAT | libc::O_CLOEXEC | libc::O_RDWR | libc::O_TRUNC,
                0o600,
            )
        };
        assert!(fd >= 0);

        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as libc::c_short;
        lock.l_whence = libc::SEEK_SET as libc::c_short;
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETLK, &lock) }, 0);

        lock.l_type = libc::F_UNLCK as libc::c_short;
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETLK, &lock) }, 0);
        assert_eq!(unsafe { libc::close(fd) }, 0);
        assert_eq!(unsafe { libc::unlink(path.as_ptr().cast()) }, 0);
    });
}

#[test]
fn bound_port_survives_closing_dup_alias() {
    const LINUX_PID_LIMIT_EXCLUSIVE: u32 = 1 << 22;

    let host_pid = std::process::id();
    assert!(
        host_pid < LINUX_PID_LIMIT_EXCLUSIVE,
        "host PID {host_pid} exceeds the 22-bit Linux PID limit"
    );
    let loopback_address = [
        127,
        0x80 | ((host_pid >> 16) as u8),
        ((host_pid >> 8) & 0xff) as u8,
        (host_pid & 0xff) as u8,
    ];
    // Every address in 127.0.0.0/8 is loopback on Linux. Use 127.128.0.0/10
    // for this test, encoding every bit of Linux's 22-bit PID limit so
    // independent host test processes do not bind the same address and port.
    // Repetitions inside this test process retain one address and still exercise
    // the same deterministic port sequence.
    det_test_fn_sequential_without_pmu(move || {
        fn bind_loopback(fd: libc::c_int, address_bytes: [u8; 4], port: u16) -> libc::c_int {
            let mut address = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: port.to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(address_bytes),
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

        fn socket_name(fd: libc::c_int) -> ([u8; 4], u16) {
            let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            let mut length = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            assert_eq!(
                unsafe {
                    libc::getsockname(
                        fd,
                        (&mut address as *mut libc::sockaddr_in).cast(),
                        &mut length,
                    )
                },
                0
            );
            assert_eq!(length as usize, std::mem::size_of::<libc::sockaddr_in>());
            assert_eq!(address.sin_family, libc::AF_INET as libc::sa_family_t);
            (
                address.sin_addr.s_addr.to_ne_bytes(),
                address.sin_port.to_be(),
            )
        }

        fn socket_option(fd: libc::c_int, option: libc::c_int) -> libc::c_int {
            let mut value = 0;
            let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            assert_eq!(
                unsafe {
                    libc::getsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        option,
                        (&mut value as *mut libc::c_int).cast(),
                        &mut length,
                    )
                },
                0
            );
            assert_eq!(length as usize, std::mem::size_of::<libc::c_int>());
            value
        }

        fn socket_identity(fd: libc::c_int) -> (libc::dev_t, libc::ino_t) {
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0);
            (stat.st_dev, stat.st_ino)
        }

        let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(socket >= 0);
        let mut first_bound = false;
        for _ in 0..128 {
            if bind_loopback(socket, loopback_address, 0) == 0 {
                first_bound = true;
                break;
            }
            assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EADDRINUSE);
        }
        assert!(first_bound, "no deterministic ephemeral port was available");
        let first_name = socket_name(socket);
        let first_identity = socket_identity(socket);
        assert_eq!(first_name.0, loopback_address);
        assert_ne!(first_name.1, 0);

        let duplicate = unsafe { libc::dup(socket) };
        assert!(duplicate >= 0);
        assert_eq!(socket_identity(duplicate), first_identity);
        assert_eq!(unsafe { libc::close(socket) }, 0);

        assert_ne!(unsafe { libc::fcntl(duplicate, libc::F_GETFD) }, -1);
        assert_eq!(socket_identity(duplicate), first_identity);
        assert_eq!(socket_name(duplicate), first_name);
        assert_eq!(socket_option(duplicate, libc::SO_TYPE), libc::SOCK_STREAM);
        assert_eq!(socket_option(duplicate, libc::SO_ERROR), 0);

        let second = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(second >= 0);
        let second_bind = bind_loopback(second, loopback_address, 0);
        let second_errno = (second_bind == -1).then(nix::errno::Errno::last);
        assert_eq!(
            second_bind, 0,
            "closing one dup alias must not free its bound port reservation: {second_errno:?}"
        );
        let second_name = socket_name(second);
        assert_eq!(second_name.0, loopback_address);
        assert_ne!(second_name.1, first_name.1);
        eprintln!(
            "bound-port state after first alias close: first={:?}:{}, next={:?}:{}, second_bind={}, second_errno={:?}",
            first_name.0, first_name.1, second_name.0, second_name.1, second_bind, second_errno
        );

        assert_eq!(unsafe { libc::close(duplicate) }, 0);

        let reuse = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(reuse >= 0);
        assert_eq!(
            bind_loopback(reuse, loopback_address, first_name.1),
            0,
            "the kernel must release the bound port after the final alias closes"
        );
        assert_eq!(socket_name(reuse), first_name);

        assert_eq!(unsafe { libc::close(reuse) }, 0);
        assert_eq!(unsafe { libc::close(second) }, 0);
    });
}

#[test]
fn unix_autobind_names_are_deterministic() {
    det_test_fn_sequential_without_pmu(|| {
        let mut open_fds = Vec::new();
        let mut names = Vec::new();
        for (socket_type, label) in [
            (libc::SOCK_DGRAM, "dgram"),
            (libc::SOCK_STREAM, "stream"),
            (libc::SOCK_SEQPACKET, "seqpacket"),
        ] {
            let fd = unsafe { libc::socket(libc::AF_UNIX, socket_type, 0) };
            assert!(fd >= 0, "{label} socket creation failed");

            let mut requested: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            requested.sun_family = libc::AF_UNIX as libc::sa_family_t;
            assert_eq!(
                unsafe {
                    libc::bind(
                        fd,
                        (&requested as *const libc::sockaddr_un).cast(),
                        std::mem::offset_of!(libc::sockaddr_un, sun_path) as libc::socklen_t,
                    )
                },
                0,
                "{label} autobind failed"
            );

            let mut observed: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            let mut observed_len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            assert_eq!(
                unsafe {
                    libc::getsockname(
                        fd,
                        (&mut observed as *mut libc::sockaddr_un).cast(),
                        &mut observed_len,
                    )
                },
                0,
                "{label} getsockname failed"
            );

            assert_eq!(observed.sun_family, libc::AF_UNIX as libc::sa_family_t);
            assert_eq!(
                observed_len as usize,
                std::mem::offset_of!(libc::sockaddr_un, sun_path) + 6
            );
            assert_eq!(observed.sun_path[0], 0);
            let name = observed.sun_path[1..6]
                .iter()
                .map(|byte| *byte as u8)
                .collect::<Vec<_>>();
            assert!(
                name.iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
            );
            println!("{label}={}", String::from_utf8(name).unwrap());
            open_fds.push(fd);
            names.push(observed.sun_path[1..6].to_vec());
        }

        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            3,
            "live autobind sockets must have unique names"
        );
        for fd in open_fds {
            assert_eq!(unsafe { libc::close(fd) }, 0);
        }
    });
}

#[test]
fn netlink_autobind_port_ids_are_deterministic() {
    det_test_fn_sequential_without_pmu(|| {
        fn bind_netlink(protocol: libc::c_int) -> (libc::c_int, u32) {
            let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, protocol) };
            assert!(fd >= 0, "Netlink socket creation failed for {protocol}");

            let mut requested: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
            requested.nl_family = libc::AF_NETLINK as libc::sa_family_t;
            assert_eq!(
                unsafe {
                    libc::bind(
                        fd,
                        (&requested as *const libc::sockaddr_nl).cast(),
                        std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
                    )
                },
                0,
                "Netlink autobind failed for {protocol}"
            );

            let mut observed: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
            let mut observed_len = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
            assert_eq!(
                unsafe {
                    libc::getsockname(
                        fd,
                        (&mut observed as *mut libc::sockaddr_nl).cast(),
                        &mut observed_len,
                    )
                },
                0,
                "Netlink getsockname failed for {protocol}"
            );
            assert_eq!(
                observed_len as usize,
                std::mem::size_of::<libc::sockaddr_nl>()
            );
            assert_eq!(observed.nl_family, libc::AF_NETLINK as libc::sa_family_t);
            assert_ne!(observed.nl_pid, 0);
            assert_eq!(observed.nl_groups, 0);
            (fd, observed.nl_pid)
        }

        for (protocol, label) in [
            (libc::NETLINK_ROUTE, "route"),
            (libc::NETLINK_USERSOCK, "usersock"),
            (libc::NETLINK_GENERIC, "generic"),
        ] {
            let (first_fd, first_port_id) = bind_netlink(protocol);
            let (second_fd, second_port_id) = bind_netlink(protocol);
            assert_ne!(first_port_id, second_port_id);
            println!("{label}={first_port_id},{second_port_id}");
            assert_eq!(unsafe { libc::close(first_fd) }, 0);
            assert_eq!(unsafe { libc::close(second_fd) }, 0);
        }
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
        max_timeslice: None,
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
