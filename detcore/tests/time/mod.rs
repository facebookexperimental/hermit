/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Tests time-related functionality of detcore.

use std::mem::MaybeUninit;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time;

use chrono::DateTime;
use chrono::Utc;
use detcore::Detcore;
use detcore::types::NANOS_PER_RCB;
use reverie::Rdtsc;
use reverie::RdtscResult;
use reverie_ptrace::testing::check_fn_with_config;

// Keep this synchronized with the clock-query category in `syscall_time`.
const NANOS_PER_CLOCK_GETTIME: f64 = 10_000.0;

#[global_allocator]
static ALLOC: test_allocator::Global = test_allocator::Global;

fn diff_millis(t1: DateTime<Utc>, t2: DateTime<Utc>) -> i64 {
    let m1 = t1.timestamp() * 1_000 + t1.timestamp_subsec_millis() as i64;
    let m2 = t2.timestamp() * 1_000 + t2.timestamp_subsec_millis() as i64;
    m2 - m1
}

fn diff_nanos(t1: DateTime<Utc>, t2: DateTime<Utc>) -> i64 {
    let m1 = t1.timestamp() * 1_000_000 + t1.timestamp_subsec_nanos() as i64;
    let m2 = t2.timestamp() * 1_000_000 + t2.timestamp_subsec_nanos() as i64;
    m2 - m1
}

#[test]
fn tod_from_epoch() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    let epoch = config.epoch;
    check_fn_with_config::<Detcore, _>(
        || {
            let now = Utc::now();
            // However exactly we compute logical time, this should be within a small
            // fraction of a (logical) second of epoch:
            assert!(diff_millis(now, epoch) < 100);
        },
        config,
        true,
    );
}

#[test]
fn tod_is_stable() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            let now = time::Instant::now();
            let x = now.elapsed();
            let y = now.elapsed();
            println!(
                "Deltas between consecutive gettime syscalls: {:?} {:?}",
                x, y
            );
            // RCBs should guarantee these are non-equal
            assert_ne!(2 * x, y);
        },
        config,
        true,
    );
}

#[test]
fn tod_gettimeofday() {
    let mut tp: MaybeUninit<libc::timeval> = MaybeUninit::uninit();
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    let epoch = config.epoch;
    check_fn_with_config::<Detcore, _>(
        || {
            assert_eq!(
                unsafe { libc::gettimeofday(tp.as_mut_ptr(), ptr::null_mut()) },
                0
            );
            let tp = unsafe { tp.assume_init() };
            let dt = DateTime::from_timestamp(tp.tv_sec, 1000 * tp.tv_usec as u32).unwrap();
            // However exactly we compute logical time, this should be within a small
            // fraction of a (logical) second of epoch:
            assert!(diff_millis(dt, epoch) < 100);
        },
        config,
        true,
    );
}

fn raw_getimeofday_delta() {
    let dt1 = {
        let mut tp: MaybeUninit<libc::timeval> = MaybeUninit::uninit();
        assert_eq!(
            unsafe { libc::gettimeofday(tp.as_mut_ptr(), ptr::null_mut()) },
            0
        );
        let tp = unsafe { tp.assume_init() };
        DateTime::from_timestamp(tp.tv_sec, 1000 * tp.tv_usec as u32).unwrap()
    };
    let dt2 = {
        let mut tp: MaybeUninit<libc::timeval> = MaybeUninit::uninit();
        assert_eq!(
            unsafe { libc::gettimeofday(tp.as_mut_ptr(), ptr::null_mut()) },
            0
        );
        let tp = unsafe { tp.assume_init() };
        DateTime::from_timestamp(tp.tv_sec, 1000 * tp.tv_usec as u32).unwrap()
    };

    let delta_ns = diff_nanos(dt1, dt2);
    println!(
        "Delta between two consecutive gettimeofday calls: {}",
        delta_ns,
    );
    // Rough expectations for the virtual time used by one gettimeofday syscall:
    assert!(delta_ns > 1000);
    assert!(delta_ns < 1_000_000_000);
}

mod tod_gettimeofday_delta {
    detcore_testutils::basic_det_test!(
        super::raw_getimeofday_delta,
        |cfg: &detcore::Config| cfg.virtualize_time,
        "all"
    );
}

#[test]
fn tod_time() {
    let mut tloc: i64 = 0;
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    let epoch = config.epoch;
    check_fn_with_config::<Detcore, _>(
        || {
            let t = unsafe { libc::time(&mut tloc as *mut i64) };
            assert_eq!(t, tloc);
            let dt = DateTime::from_timestamp(t, 0).unwrap();
            assert_eq!(dt.timestamp(), epoch.timestamp());
        },
        config,
        true,
    );
}

#[test]
/// Check that the initially observed time is still epoch.  This is a bit fragile, because
/// it requires that the clock_gettime call be the VERY first instruction/syscall counted
/// within the new process.
fn tod_clock_gettime() {
    let mut tp: MaybeUninit<libc::timespec> = MaybeUninit::uninit();
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    let epoch = config.epoch;
    check_fn_with_config::<Detcore, _>(
        || {
            assert_eq!(
                unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, tp.as_mut_ptr()) },
                0
            );
            let tp = unsafe { tp.assume_init() };
            let dt = DateTime::from_timestamp(tp.tv_sec, tp.tv_nsec as u32).unwrap();
            // However exactly we compute logical time, this should be within a small
            // fraction of a (logical) second of epoch:
            assert!(diff_millis(dt, epoch) < 100);
        },
        config,
        true,
    );
}

#[test]
fn target_timeslice_yields_at_syscall_boundaries_without_pmu() {
    let config = detcore::Config {
        virtualize_time: true,
        max_timeslice: None,
        target_timeslice: std::num::NonZeroU64::new(100_000),
        sequentialize_threads: true,
        no_rcb_time: true,
        // Cancel no_rcb_time's 500x fallback so the target is literal virtual nanoseconds.
        clock_multiplier: Some(1.0 / 500.0),
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            let read_time = || {
                let mut now = MaybeUninit::<libc::timespec>::uninit();
                let result = unsafe {
                    libc::syscall(
                        libc::SYS_clock_gettime,
                        libc::CLOCK_MONOTONIC,
                        now.as_mut_ptr(),
                    )
                };
                assert_eq!(result, 0);
                unsafe { now.assume_init() }
            };

            let done = Arc::new(AtomicBool::new(false));
            let worker_done = Arc::clone(&done);
            let worker = thread::spawn(move || {
                thread::sleep(time::Duration::from_millis(1));
                worker_done.store(true, Ordering::Release);
            });

            let mut calls = 0;
            while !done.load(Ordering::Acquire) && calls < 1_000 {
                read_time();
                calls += 1;
            }

            assert!(
                done.load(Ordering::Acquire),
                "clock_gettime loop starved its peer for {calls} calls"
            );
            worker.join().unwrap();
        },
        config,
        true,
    );
}

#[test]
fn max_timeslice_preempts_cpu_bound_code_without_rcb_logical_time() {
    let config = detcore::Config {
        virtualize_time: true,
        max_timeslice: std::num::NonZeroU64::new(1_000_000),
        target_timeslice: None,
        sequentialize_threads: true,
        no_rcb_time: true,
        clock_multiplier: Some(1.0),
        record_preemptions: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            let start = Arc::new(AtomicBool::new(false));
            let done = Arc::new(AtomicBool::new(false));
            let worker_start = Arc::clone(&start);
            let worker_done = Arc::clone(&done);
            let worker = thread::spawn(move || {
                while !worker_start.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                worker_done.store(true, Ordering::Release);
            });

            start.store(true, Ordering::Release);
            let mut spins = 0;
            while !done.load(Ordering::Acquire) && spins < 50_000_000 {
                std::hint::spin_loop();
                spins += 1;
            }

            assert!(
                done.load(Ordering::Acquire),
                "PMU maximum did not schedule the peer after {spins} spins"
            );
            worker.join().unwrap();
        },
        config,
        true,
    );
}

#[test]
fn tod_clock_getres() {
    let mut tp: MaybeUninit<libc::timespec> = MaybeUninit::uninit();
    let config = detcore::Config {
        clock_multiplier: Some(1_234_567.0),
        virtualize_time: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            assert_eq!(
                unsafe { libc::clock_getres(libc::CLOCK_MONOTONIC, tp.as_mut_ptr()) },
                0
            );
            let tp = unsafe { tp.assume_init() };
            assert_eq!(tp.tv_sec, 0);
            assert_eq!(tp.tv_nsec, 10000); // Rgiht now the res is CONSTANT.
        },
        config,
        true,
    );
}

// Regression: a NULL `res` pointer is valid for clock_getres (the kernel
// validates the clockid and returns 0 without storing the resolution). GHC's
// threaded RTS probes the per-thread CPU clock this way in
// getCurrentThreadCPUTime; returning EFAULT here spuriously aborts the guest.
#[test]
fn clock_getres_null_res_is_ok() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            assert_eq!(
                unsafe { libc::clock_getres(libc::CLOCK_MONOTONIC, std::ptr::null_mut()) },
                0
            );
            assert_eq!(
                unsafe { libc::clock_getres(libc::CLOCK_THREAD_CPUTIME_ID, std::ptr::null_mut()) },
                0
            );
        },
        config,
        true,
    );
}

#[test]
fn tod_clock_getres_2() {
    let multiplier = 1000.0;
    let config = detcore::Config {
        clock_multiplier: Some(multiplier),
        virtualize_time: true,
        ..Default::default()
    };
    let sequentialize = config.sequentialize_threads;
    let timeout_disabled = config.max_timeslice.is_none();
    check_fn_with_config::<Detcore, _>(
        || {
            let now = time::Instant::now();
            // Spot check a single syscall clock delta (clock_gettime).
            let nanos = now.elapsed().as_nanos();
            let expected = if sequentialize && timeout_disabled {
                // Additional multiplier, see DetTime::new():
                500 * (multiplier * NANOS_PER_CLOCK_GETTIME) as u128
            } else {
                (multiplier * NANOS_PER_CLOCK_GETTIME) as u128
            };
            // account for some slop from RCBs
            assert!(nanos >= expected);
            assert!(nanos < expected + 10 * ((multiplier * NANOS_PER_RCB) as u128));
        },
        config,
        true,
    );
}

#[test]
fn rdtsc_deltas() {
    let config = detcore::Config {
        clock_multiplier: Some(12345.0),
        virtualize_time: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            let tsc1 = RdtscResult::new(Rdtsc::Tsc).tsc;
            let tsc2 = RdtscResult::new(Rdtsc::Tsc).tsc;
            println!(
                "Consecutive raw rdtscs: {} {},  delta: {}",
                tsc1,
                tsc2,
                tsc2 - tsc1
            );
            // Whatever the delta is, it has to have stepped by AT LEAST the multiplier:
            assert!(tsc2 - tsc1 > 12345);
        },
        config,
        true,
    );
}

// A periodic timerfd is armed and read against detcore's virtual clock (not host
// wall-clock) when threads are sequentialized and time is virtualized. This is
// the mechanism that determinizes GHC's RTS context-switch ticker, which is a
// periodic timerfd whose blocking read() on a dedicated thread drives green-
// thread preemption. gettime must reflect the virtual arming, and each blocking
// read must report at least one expiration and return the 8-byte count.
#[test]
fn timerfd_periodic_virtual_time() {
    const PERIOD_NS: i64 = 10_000_000; // 10ms

    let config = detcore::Config {
        sequentialize_threads: true,
        virtualize_time: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, 0) };
            assert!(fd >= 0, "timerfd_create failed: {}", unsafe {
                *libc::__errno_location()
            });

            // Arm as a 10ms periodic timer (relative initial expiration).
            let new = libc::itimerspec {
                it_interval: libc::timespec {
                    tv_sec: 0,
                    tv_nsec: PERIOD_NS,
                },
                it_value: libc::timespec {
                    tv_sec: 0,
                    tv_nsec: PERIOD_NS,
                },
            };
            let rc = unsafe { libc::timerfd_settime(fd, 0, &new, ptr::null_mut()) };
            assert_eq!(rc, 0, "timerfd_settime failed");

            // gettime reflects the virtual arming: interval preserved, remaining
            // in (0, PERIOD].
            let mut cur = MaybeUninit::<libc::itimerspec>::zeroed();
            let rc = unsafe { libc::timerfd_gettime(fd, cur.as_mut_ptr()) };
            assert_eq!(rc, 0, "timerfd_gettime failed");
            let cur = unsafe { cur.assume_init() };
            assert_eq!(cur.it_interval.tv_sec, 0);
            assert_eq!(cur.it_interval.tv_nsec, PERIOD_NS);
            let remaining = cur.it_value.tv_sec * 1_000_000_000 + cur.it_value.tv_nsec;
            assert!(
                remaining > 0 && remaining <= PERIOD_NS,
                "unexpected remaining ns: {}",
                remaining
            );

            // Three back-to-back blocking reads: each is descheduled as a timed
            // waiter and woken at *exactly* the virtual deadline, so each read
            // observes exactly one elapsed period and reports exactly one
            // expiration. This exact count (not merely `>= 1`) is the load-immune
            // determinism guarantee: it is a pure function of the virtual arming
            // and virtual time, independent of host wall-clock or scheduling.
            for i in 0..3 {
                let mut expirations: u64 = 0;
                let n = unsafe {
                    libc::read(
                        fd,
                        &mut expirations as *mut u64 as *mut libc::c_void,
                        std::mem::size_of::<u64>(),
                    )
                };
                assert_eq!(n, 8, "timerfd read #{} returned {} bytes", i, n);
                assert_eq!(
                    expirations, 1,
                    "timerfd read #{} reported {} expirations (expected exactly 1)",
                    i, expirations
                );
            }

            assert_eq!(unsafe { libc::close(fd) }, 0);
        },
        config,
        true,
    );
}

// Guest body for the cross-run determinism test: arm a periodic timerfd and emit
// the exact (expiration-count, remaining-ns) pair after each of several blocking
// reads. Every value printed is a pure function of the virtual arming and the
// virtual clock, so the whole stdout stream must be byte-identical across runs.
fn timerfd_periodic_sequence_guest() {
    const PERIOD_NS: i64 = 10_000_000; // 10ms

    let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, 0) };
    assert!(fd >= 0, "timerfd_create failed: {}", unsafe {
        *libc::__errno_location()
    });
    let new = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: PERIOD_NS,
        },
        it_value: libc::timespec {
            tv_sec: 0,
            tv_nsec: PERIOD_NS,
        },
    };
    assert_eq!(
        unsafe { libc::timerfd_settime(fd, 0, &new, ptr::null_mut()) },
        0,
        "timerfd_settime failed"
    );

    for _ in 0..5 {
        let mut expirations: u64 = 0;
        let n = unsafe {
            libc::read(
                fd,
                &mut expirations as *mut u64 as *mut libc::c_void,
                std::mem::size_of::<u64>(),
            )
        };
        assert_eq!(n, 8, "timerfd read returned {} bytes", n);

        let mut cur = MaybeUninit::<libc::itimerspec>::zeroed();
        assert_eq!(
            unsafe { libc::timerfd_gettime(fd, cur.as_mut_ptr()) },
            0,
            "timerfd_gettime failed"
        );
        let cur = unsafe { cur.assume_init() };
        let remaining = cur.it_value.tv_sec * 1_000_000_000 + cur.it_value.tv_nsec;
        // Fold both the delivered count and the virtual clock into stdout.
        println!("expirations={} remaining={}", expirations, remaining);
    }
    assert_eq!(unsafe { libc::close(fd) }, 0);
}

// L2-style determinism: the full (expiration-count, remaining-ns) sequence a
// periodic timerfd produces must be byte-for-byte identical across independent
// runs. This is the load-immune bitwise check finding #8 asked for; the single
// `check_fn_with_config` run above establishes functional correctness, and this
// establishes cross-run reproducibility. Mirrors the `run_five_times` harness in
// tests/misc/notification_fds.rs.
#[test]
fn timerfd_periodic_sequence_is_deterministic_across_runs() {
    let config = detcore::Config {
        sequentialize_threads: true,
        virtualize_time: true,
        ..Default::default()
    };
    let mut expected: Option<Vec<u8>> = None;
    for run in 1..=5 {
        let (output, _state) = detcore_testutils::test_fn_with_config::<Detcore, _>(
            timerfd_periodic_sequence_guest,
            config.clone(),
            true,
        )
        .unwrap_or_else(|error| panic!("timerfd guest run {run} failed: {error:#}"));
        assert_eq!(
            output.status,
            reverie::ExitStatus::Exited(0),
            "timerfd guest run {run} did not exit cleanly: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.stdout.is_empty(),
            "timerfd guest run {run} produced no stdout"
        );
        match &expected {
            Some(exp) => assert_eq!(
                &output.stdout, exp,
                "timerfd expiration/remaining sequence diverged on run {run}"
            ),
            None => expected = Some(output.stdout),
        }
    }
}

// Finding #5 regression: once a periodic timer is overdue (now >= deadline) but
// its expirations have not been read, gettime must project the *next* future
// expiration rather than reporting zero. Advance the virtual clock past several
// periods via nanosleep without reading, then require a strictly-positive
// remaining in (0, PERIOD].
#[test]
fn timerfd_overdue_periodic_gettime_reports_next_expiration() {
    const PERIOD_NS: i64 = 10_000_000; // 10ms

    let config = detcore::Config {
        sequentialize_threads: true,
        virtualize_time: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, 0) };
            assert!(fd >= 0);
            let new = libc::itimerspec {
                it_interval: libc::timespec {
                    tv_sec: 0,
                    tv_nsec: PERIOD_NS,
                },
                it_value: libc::timespec {
                    tv_sec: 0,
                    tv_nsec: PERIOD_NS,
                },
            };
            assert_eq!(
                unsafe { libc::timerfd_settime(fd, 0, &new, ptr::null_mut()) },
                0,
                "timerfd_settime failed"
            );

            // Sleep 3.5 periods on the virtual clock without reading the timer.
            let req = libc::timespec {
                tv_sec: 0,
                tv_nsec: 35 * PERIOD_NS / 10,
            };
            assert_eq!(
                unsafe { libc::nanosleep(&req, ptr::null_mut()) },
                0,
                "nanosleep failed"
            );

            let mut cur = MaybeUninit::<libc::itimerspec>::zeroed();
            assert_eq!(
                unsafe { libc::timerfd_gettime(fd, cur.as_mut_ptr()) },
                0,
                "timerfd_gettime failed"
            );
            let cur = unsafe { cur.assume_init() };
            let remaining = cur.it_value.tv_sec * 1_000_000_000 + cur.it_value.tv_nsec;
            assert!(
                remaining > 0 && remaining <= PERIOD_NS,
                "overdue periodic gettime reported {} ns (expected next expiration in (0, PERIOD])",
                remaining
            );
            assert_eq!(
                cur.it_interval.tv_nsec, PERIOD_NS,
                "interval must survive overdue gettime"
            );
            assert_eq!(unsafe { libc::close(fd) }, 0);
        },
        config,
        true,
    );
}

// Finding #6 regression: a TFD_NONBLOCK timerfd whose next expiration has not
// been reached must return EAGAIN on read (never EFAULT, never block), matching
// the kernel's EINVAL -> EAGAIN -> EFAULT ordering. A null buffer must not turn
// the unexpired-nonblocking case into EFAULT.
#[test]
fn timerfd_nonblocking_unexpired_returns_eagain() {
    let config = detcore::Config {
        sequentialize_threads: true,
        virtualize_time: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK) };
            assert!(fd >= 0);
            // Armed one second out, so it is unexpired at the immediate read.
            let new = libc::itimerspec {
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
                unsafe { libc::timerfd_settime(fd, 0, &new, ptr::null_mut()) },
                0,
                "timerfd_settime failed"
            );

            let mut expirations: u64 = 0;
            let n = unsafe {
                libc::read(
                    fd,
                    &mut expirations as *mut u64 as *mut libc::c_void,
                    std::mem::size_of::<u64>(),
                )
            };
            assert_eq!(n, -1, "unexpired nonblocking read should fail");
            assert_eq!(
                unsafe { *libc::__errno_location() },
                libc::EAGAIN,
                "unexpired nonblocking read must return EAGAIN"
            );
            assert_eq!(unsafe { libc::close(fd) }, 0);
        },
        config,
        true,
    );
}
