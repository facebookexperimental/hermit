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
use std::time;

use chrono::DateTime;
use chrono::Utc;
use detcore::types::NANOS_PER_RCB;
use detcore::types::NANOS_PER_SYSCALL;
use detcore::Detcore;
use reverie::Rdtsc;
use reverie::RdtscResult;
use reverie_ptrace::testing::check_fn_with_config;

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

#[test]
fn tod_clock_getres_2() {
    let multiplier = 1000.0;
    let config = detcore::Config {
        clock_multiplier: Some(multiplier),
        virtualize_time: true,
        ..Default::default()
    };
    let sequentialize = config.sequentialize_threads;
    let timeout_disabled = config.preemption_timeout.is_none();
    check_fn_with_config::<Detcore, _>(
        || {
            let now = time::Instant::now();
            // Spot check a single syscall clock delta (clock_gettime).
            let nanos = now.elapsed().as_nanos();
            let expected = if sequentialize && timeout_disabled {
                // Additional multiplier, see DetTime::new():
                500 * (multiplier * NANOS_PER_SYSCALL) as u128
            } else {
                (multiplier * NANOS_PER_SYSCALL) as u128
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
