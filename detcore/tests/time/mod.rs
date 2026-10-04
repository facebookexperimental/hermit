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
use detcore::types::NANOS_PER_SYSCALL;
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

/// `/proc/stat` `btime` is the fixed boot instant, so a fractional epoch must
/// not make it move while the guest merely runs and sleeps. Deriving it as
/// `floor(now) - floor(now - boot)` let the two floors round independently,
/// and the reviewed reproducer at this epoch printed 1767225480, 1767225481,
/// ..., 1767225480 across seven samples.
#[test]
fn proc_stat_btime_is_fixed_for_a_fractional_epoch() {
    let config = detcore::Config {
        virtualize_time: true,
        epoch: "2026-01-01T00:00:00.750Z".parse().unwrap(),
        // The scheduler is what turns a sleep into elapsed logical time; the
        // PMU is not needed for that.
        sequentialize_threads: true,
        max_timeslice: None,
        ..Default::default()
    };
    let expected_btime = config.epoch.timestamp() - config.sysinfo_uptime_offset as i64;
    check_fn_with_config::<Detcore, _>(
        move || {
            let read_btime = || -> i64 {
                let stat = std::fs::read_to_string("/proc/stat").unwrap();
                let line = stat
                    .lines()
                    .find(|line| line.starts_with("btime "))
                    .unwrap();
                line["btime ".len()..].parse().unwrap()
            };
            let first_second = Utc::now().timestamp();
            let samples: Vec<i64> = (0..7)
                .map(|_| {
                    let btime = read_btime();
                    thread::sleep(time::Duration::from_millis(300));
                    btime
                })
                .collect();
            // The samples must straddle absolute-second boundaries, or this
            // test could not observe the rounding it guards against.
            assert!(Utc::now().timestamp() >= first_second + 2);
            assert_eq!(samples, vec![expected_btime; 7]);
        },
        config,
        true,
    );
}

/// Config accepts every u64 `sysinfo_uptime_offset`. An offset of 2^63 still
/// places the boot instant inside time64_t (2026-01-01 minus 2^63 s is
/// -9223372035087550208), so `/proc/stat` must render it exactly, and files
/// that never show `btime`, such as `/proc/uptime` and `/proc/meminfo`, must
/// not depend on it at all. Only a boot instant below `i64::MIN` seconds is
/// unrepresentable, and that refuses `/proc/stat` alone.
#[test]
fn procfs_reads_accept_every_uptime_offset() {
    const EPOCH_SECONDS: u64 = 1_767_225_600;
    for (offset, expected_btime) in [
        (120, Some(1_767_225_480)),
        (1 << 63, Some(-9_223_372_035_087_550_208)),
        (EPOCH_SECONDS + (1 << 63), Some(i64::MIN)),
        (EPOCH_SECONDS + (1 << 63) + 1, None),
    ] {
        let config = detcore::Config {
            virtualize_time: true,
            epoch: "2026-01-01T00:00:00Z".parse().unwrap(),
            sysinfo_uptime_offset: offset,
            ..Default::default()
        };
        check_fn_with_config::<Detcore, _>(
            move || {
                // Logical time starts at the epoch, and the guest has used far
                // less than a second of it by this first read.
                let uptime = std::fs::read_to_string("/proc/uptime").unwrap();
                assert_eq!(uptime, format!("{offset}.00 0.00\n"));
                let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap();
                assert!(meminfo.starts_with("MemTotal:"), "{meminfo:?}");
                let stat = std::fs::read_to_string("/proc/stat");
                match expected_btime {
                    Some(expected) => {
                        let stat = stat.unwrap();
                        let line = stat
                            .lines()
                            .find(|line| line.starts_with("btime "))
                            .unwrap();
                        assert_eq!(line["btime ".len()..].parse::<i64>().unwrap(), expected);
                    }
                    None => assert_eq!(stat.unwrap_err().raw_os_error(), Some(libc::EOVERFLOW)),
                }
            },
            config,
            true,
        );
    }
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

/// `rdtsc` and `clock_gettime` must name the same instant.
///
/// They used to read two different clocks: `rdtsc` returned the calling
/// thread's own logical time while `clock_gettime` returned the coordinator's,
/// which is the sum over threads. A guest comparing them -- a clocksource
/// watchdog, a delay loop calibrated against a device timer -- saw two clocks
/// disagreeing by milliseconds and diverging in rate with the thread count.
///
/// Sampling repeatedly rather than once is deliberate: agreeing on a first read
/// while drifting afterwards is the failure this is meant to catch.
#[test]
fn rdtsc_agrees_with_clock_gettime() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            // One intercepted syscall's worth of virtual time separates the two
            // reads, plus slack for the retired branches between them.
            let tolerance = 100 * NANOS_PER_SYSCALL as u64;
            for i in 0..8 {
                let tsc = RdtscResult::new(Rdtsc::Tsc).tsc;
                let mut ts = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                assert_eq!(
                    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) },
                    0
                );
                let mono = ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64;
                let gap = mono.abs_diff(tsc);
                println!(
                    "sample {}: rdtsc {} clock_gettime {} gap {}",
                    i, tsc, mono, gap
                );
                assert!(
                    gap < tolerance,
                    "rdtsc and clock_gettime are {} ns apart on sample {}, which is more \
                     than the {} ns a single intercepted syscall accounts for; they are \
                     reading different clocks again",
                    gap,
                    i,
                    tolerance,
                );
            }
        },
        config,
        true,
    );
}

/// A malformed `timespec` must fail EINVAL, not become an indefinite sleep.
///
/// Detcore fed `Timespec`'s signed fields through `as u64`, so `tv_sec = -1`
/// wrapped to `u64::MAX` and produced `SleepUntil(INDEFINITE)`. The only guest
/// thread then parked with no deadline, the run queue emptied, and the
/// scheduler deliberately does not jump the clock for an indefinite waiter, so
/// the container died instead of returning an errno.
///
/// Both directions matter here, which is why the past-absolute case is in the
/// same test: rejecting a malformed field must not also reject an early
/// deadline. Measured against native Linux on x86_64: all three malformed
/// shapes give EINVAL, and a past absolute deadline gives 0.
#[test]
fn nanosleep_rejects_malformed_timespec_but_not_a_past_deadline() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            // `clock_nanosleep` returns the error directly rather than via errno.
            let sleep = |sec: i64, nsec: i64, flags: libc::c_int| -> libc::c_int {
                let ts = libc::timespec {
                    tv_sec: sec,
                    tv_nsec: nsec,
                };
                unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, flags, &ts, ptr::null_mut()) }
            };

            // Malformed: negative seconds is the case that used to hang.
            assert_eq!(sleep(-1, 0, 0), libc::EINVAL, "relative tv_sec=-1");
            assert_eq!(sleep(0, -1, 0), libc::EINVAL, "relative tv_nsec=-1");
            assert_eq!(
                sleep(0, 1_000_000_000, 0),
                libc::EINVAL,
                "relative tv_nsec out of range"
            );
            assert_eq!(
                sleep(-1, 0, libc::TIMER_ABSTIME),
                libc::EINVAL,
                "absolute tv_sec=-1"
            );

            // Well-formed, and must still succeed: a zero interval, and an
            // absolute deadline already in the past. Neither is an error on
            // Linux, so a fix that rejected them would be too aggressive.
            assert_eq!(sleep(0, 0, 0), 0, "zero relative interval");
            assert_eq!(
                sleep(1, 0, libc::TIMER_ABSTIME),
                0,
                "past absolute deadline"
            );
        },
        config,
        true,
    );
}
