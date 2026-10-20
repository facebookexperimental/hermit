/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#![allow(
    unexpected_cfgs,
    reason = "`fbcode_build` is supplied by the internal Buck build"
)]

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
use reverie_ptrace::testing::test_fn_with_config;

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
    let m1 = t1.timestamp() * 1_000_000_000 + t1.timestamp_subsec_nanos() as i64;
    let m2 = t2.timestamp() * 1_000_000_000 + t2.timestamp_subsec_nanos() as i64;
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
            let delta_ms = diff_millis(epoch, now);
            // However exactly we compute logical time, this should be within a small
            // fraction of a (logical) second of epoch:
            assert!(
                (0..100).contains(&delta_ms),
                "observed time {now} is not within 100 ms after epoch {epoch}: difference {delta_ms} ms"
            );
        },
        config,
        true,
    );
}

#[cfg_attr(
    fbcode_build,
    ignore = "fbsource-disabled: hangs or crashes at exit under v0.4.1; reworked upstream after v0.4.1 and re-enabled by the 0.5 import (D125030699)"
)]
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

#[cfg_attr(
    fbcode_build,
    ignore = "fbsource-disabled: hangs or crashes at exit under v0.4.1; reworked upstream after v0.4.1 and re-enabled by the 0.5 import (D125030699)"
)]
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
            let delta_ms = diff_millis(epoch, dt);
            // However exactly we compute logical time, this should be within a small
            // fraction of a (logical) second of epoch:
            assert!(
                (0..100).contains(&delta_ms),
                "gettimeofday time {dt} is not within 100 ms after epoch {epoch}: difference {delta_ms} ms"
            );
        },
        config,
        true,
    );
}

// Distinct from every virtual or host time these tests can observe (2020-06-20).
const SENTINEL_TV: libc::timeval = libc::timeval {
    tv_sec: 0x5eed_5eed,
    tv_usec: 424_242,
};

// Upper bound on how far past the epoch the virtual clock may be during the
// faulting-gettimeofday tests. Host wall-clock time is months past the default
// epoch, so this only has to separate virtual from host time.
const MAX_VIRTUAL_OFFSET_MICROS: i64 = 1_000_000;

fn timeval_micros(tv: &libc::timeval) -> i64 {
    tv.tv_sec * 1_000_000 + tv.tv_usec
}

/// Issues the raw syscall, so no vDSO path can bypass the tracer, and returns
/// its result with `errno` (zero on success).
fn raw_gettimeofday(tv: *mut libc::timeval, tz: *mut libc::c_void) -> (i64, i32) {
    let ret = unsafe { libc::syscall(libc::SYS_gettimeofday, tv, tz) };
    let errno = if ret == -1 {
        unsafe { *libc::__errno_location() }
    } else {
        0
    };
    (ret, errno)
}

fn successful_gettimeofday() -> libc::timeval {
    let mut tv = SENTINEL_TV;
    assert_eq!(raw_gettimeofday(&mut tv, ptr::null_mut()), (0, 0));
    tv
}

/// Maps `count` fresh read-write anonymous pages and returns their base and
/// the page size.
fn map_pages(count: usize) -> (*mut u8, usize) {
    let page_size = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();
    let base = unsafe {
        libc::mmap(
            ptr::null_mut(),
            count * page_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED);
    (base.cast(), page_size)
}

#[test]
/// Linux copies `tv` to user memory before `tz` and reports EFAULT if the `tz`
/// copy faults, so the host has already stored its wall clock in `tv` when the
/// call fails. The guest must still observe virtual time there: each failed
/// call's `tv` must lie between the successful virtual reads around it.
fn tod_gettimeofday_faulting_tz_writes_virtual_tv() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    let epoch_micros = config.epoch.timestamp_micros();
    check_fn_with_config::<Detcore, _>(
        || {
            let (readonly, page_size) = map_pages(1);
            assert_eq!(
                unsafe { libc::mprotect(readonly.cast(), page_size, libc::PROT_READ) },
                0
            );
            // Address 1 is below mmap_min_addr, so it can never be mapped.
            let unmapped = ptr::without_provenance_mut::<libc::c_void>(1);

            let mut unmapped_tz_tv = SENTINEL_TV;
            let mut readonly_tz_tv = SENTINEL_TV;
            let before = successful_gettimeofday();
            let unmapped_tz = raw_gettimeofday(&mut unmapped_tz_tv, unmapped);
            let between = successful_gettimeofday();
            let readonly_tz = raw_gettimeofday(&mut readonly_tz_tv, readonly.cast());
            let after = successful_gettimeofday();

            assert_eq!(unmapped_tz, (-1, libc::EFAULT));
            assert_eq!(readonly_tz, (-1, libc::EFAULT));
            let offsets = [before, unmapped_tz_tv, between, readonly_tz_tv, after]
                .map(|tv| timeval_micros(&tv) - epoch_micros);
            assert!(
                offsets.is_sorted() && offsets[0] >= 0 && offsets[4] < MAX_VIRTUAL_OFFSET_MICROS,
                "microseconds past the virtual epoch for [ok, unmapped tz, ok, read-only tz, ok]: \
                 {offsets:?}"
            );

            // A faulting `tv` fails before anything is stored.
            assert_eq!(
                raw_gettimeofday(unmapped.cast(), ptr::null_mut()),
                (-1, libc::EFAULT)
            );
            assert_eq!(unsafe { libc::munmap(readonly.cast(), page_size) }, 0);
        },
        config,
        true,
    );
}

#[test]
/// When `tv` itself faults, Linux stores whole words up to the first unwritable
/// one: a `tv` on a read-only page is left untouched, and a `tv` whose
/// `tv_usec` lies on a read-only page receives only `tv_sec`. That `tv_sec`
/// must be virtual, and nothing may be stored in the read-only page.
fn tod_gettimeofday_faulting_tv_respects_page_protection() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            let (pages, page_size) = map_pages(2);
            let readonly = unsafe { pages.add(page_size) };
            let readonly_tv = readonly.cast::<libc::timeval>();
            // `tv_sec` is the last word of the writable page and `tv_usec` the
            // first word of the read-only page, i.e. `readonly_tv.tv_sec`.
            let straddling_tv = unsafe { readonly.sub(8) }.cast::<libc::timeval>();
            unsafe {
                readonly_tv.write(SENTINEL_TV);
                (*straddling_tv).tv_sec = SENTINEL_TV.tv_sec;
            }
            assert_eq!(
                unsafe { libc::mprotect(readonly.cast(), page_size, libc::PROT_READ) },
                0
            );
            let readonly_page = || {
                let tv = unsafe { readonly_tv.read() };
                (tv.tv_sec, tv.tv_usec)
            };
            let sentinel = (SENTINEL_TV.tv_sec, SENTINEL_TV.tv_usec);

            assert_eq!(
                raw_gettimeofday(readonly_tv, ptr::null_mut()),
                (-1, libc::EFAULT)
            );
            assert_eq!(readonly_page(), sentinel, "read-only tv was written");

            let before = successful_gettimeofday();
            let straddling = raw_gettimeofday(straddling_tv, ptr::null_mut());
            let after = successful_gettimeofday();
            assert_eq!(straddling, (-1, libc::EFAULT));
            let stored_sec = unsafe { (*straddling_tv).tv_sec };
            assert!(
                (before.tv_sec..=after.tv_sec).contains(&stored_sec),
                "straddling tv_sec {stored_sec} is outside the virtual seconds {}..={}",
                before.tv_sec,
                after.tv_sec,
            );
            assert_eq!(
                readonly_page(),
                sentinel,
                "straddling tv wrote into the read-only page"
            );
            assert_eq!(unsafe { libc::munmap(pages.cast(), 2 * page_size) }, 0);
        },
        config,
        true,
    );
}

// The byte that the page-boundary tests below place around the boundary, and
// how many bytes on each side of it they fill and inspect.
const BOUNDARY_FILL: u8 = 0xa5;
const BOUNDARY_WINDOW: usize = 32;

/// The page after the boundary, into which a misaligned `tv` crosses.
#[derive(Clone, Copy, Debug)]
enum SecondPage {
    ReadOnly,
    NoAccess,
    Unmapped,
}

/// Fills the `BOUNDARY_WINDOW` bytes on each side of `boundary`.
fn fill_boundary(boundary: *mut u8) {
    unsafe {
        ptr::write_bytes(
            boundary.sub(BOUNDARY_WINDOW),
            BOUNDARY_FILL,
            2 * BOUNDARY_WINDOW,
        )
    };
}

/// Copies the `BOUNDARY_WINDOW` bytes before `boundary` and the `after` bytes
/// from it.
fn boundary_window(boundary: *mut u8, after: usize) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(boundary.sub(BOUNDARY_WINDOW), BOUNDARY_WINDOW + after) }
        .to_vec()
}

fn window_word(window: &[u8], start: usize) -> i64 {
    i64::from_ne_bytes(window[start..start + 8].try_into().unwrap())
}

fn protect(addr: *mut u8, len: usize, prot: libc::c_int) {
    assert_eq!(unsafe { libc::mprotect(addr.cast(), len, prot) }, 0);
}

#[test]
/// Linux stores each `timeval` word with one eight-byte store, which stores
/// nothing when either page it touches is unwritable. With `tv` four bytes
/// before an unwritable page, `tv_sec` crosses into it and no byte changes;
/// twelve bytes before, `tv_sec` is stored and no byte of `tv_usec` is, not
/// even the four on the writable page, which may be write-only and so cannot
/// be read back. The stored `tv_sec` must be virtual.
fn tod_gettimeofday_misaligned_tv_stores_whole_words_only() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    let epoch_micros = config.epoch.timestamp_micros();
    check_fn_with_config::<Detcore, _>(
        || {
            // Address 1 is below mmap_min_addr, so it can never be mapped.
            let unmapped = ptr::without_provenance_mut::<libc::c_void>(1);
            let read_write = libc::PROT_READ | libc::PROT_WRITE;
            // The first page's protection, the second page, and how many bytes
            // before the boundary `tv` starts.
            for (first, second, back) in [
                (read_write, SecondPage::ReadOnly, 4),
                (read_write, SecondPage::ReadOnly, 12),
                (read_write, SecondPage::NoAccess, 4),
                (read_write, SecondPage::NoAccess, 12),
                (read_write, SecondPage::Unmapped, 4),
                (read_write, SecondPage::Unmapped, 12),
                (libc::PROT_WRITE, SecondPage::NoAccess, 12),
                (libc::PROT_WRITE, SecondPage::Unmapped, 12),
            ] {
                let case = format!(
                    "tv {back} bytes before a {second:?} page, first page protection {first:#x}"
                );
                let (pages, page_size) = map_pages(2);
                let boundary = unsafe { pages.add(page_size) };
                fill_boundary(boundary);
                protect(pages, page_size, first);
                let mapped_after = match second {
                    SecondPage::ReadOnly => {
                        protect(boundary, page_size, libc::PROT_READ);
                        BOUNDARY_WINDOW
                    }
                    SecondPage::NoAccess => {
                        protect(boundary, page_size, libc::PROT_NONE);
                        BOUNDARY_WINDOW
                    }
                    SecondPage::Unmapped => {
                        assert_eq!(unsafe { libc::munmap(boundary.cast(), page_size) }, 0);
                        0
                    }
                };

                let before = successful_gettimeofday();
                let result = raw_gettimeofday(unsafe { boundary.sub(back) }.cast(), unmapped);
                let after = successful_gettimeofday();
                protect(pages, page_size, read_write);
                if let SecondPage::NoAccess = second {
                    protect(boundary, page_size, libc::PROT_READ);
                }
                let window = boundary_window(boundary, mapped_after);

                assert_eq!(result, (-1, libc::EFAULT), "{case}");
                let offsets = [before, after].map(|tv| timeval_micros(&tv) - epoch_micros);
                assert!(
                    offsets[0] >= 0 && offsets[1] < MAX_VIRTUAL_OFFSET_MICROS,
                    "{case}: microseconds past the virtual epoch for [ok, ok]: {offsets:?}"
                );
                let mut expected = vec![BOUNDARY_FILL; window.len()];
                if back == 12 {
                    let start = BOUNDARY_WINDOW - back;
                    let stored_sec = window_word(&window, start);
                    assert!(
                        (before.tv_sec..=after.tv_sec).contains(&stored_sec),
                        "{case}: tv_sec {stored_sec} is outside the virtual seconds {}..={}",
                        before.tv_sec,
                        after.tv_sec,
                    );
                    expected[start..start + 8].copy_from_slice(&stored_sec.to_ne_bytes());
                }
                assert!(
                    window == expected,
                    "{case}: the bytes from {BOUNDARY_WINDOW} before the boundary are \
                     {window:02x?}, expected {expected:02x?}"
                );
                let mapped_len = match second {
                    SecondPage::Unmapped => page_size,
                    _ => 2 * page_size,
                };
                assert_eq!(unsafe { libc::munmap(pages.cast(), mapped_len) }, 0);
            }
        },
        config,
        true,
    );
}

#[test]
/// Linux stores into memory mapped with PROT_WRITE but not PROT_READ, which
/// the tracer cannot read back, so such a `tv` holds host time when `tz`
/// faults. It must hold virtual time instead, both wholly on a write-only page
/// and crossing between a write-only page and a read-write one.
fn tod_gettimeofday_faulting_tz_write_only_tv_receives_virtual_time() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    let epoch_micros = config.epoch.timestamp_micros();
    check_fn_with_config::<Detcore, _>(
        || {
            let unmapped = ptr::without_provenance_mut::<libc::c_void>(1);
            let read_write = libc::PROT_READ | libc::PROT_WRITE;
            let write_only = libc::PROT_WRITE;
            // The first page's protection, the second page's, and how many
            // bytes before the boundary `tv` starts.
            for (first, second, back) in [
                (write_only, write_only, 32),
                (read_write, write_only, 4),
                (write_only, read_write, 4),
                (read_write, write_only, 12),
            ] {
                let case = format!(
                    "tv {back} bytes before the boundary, page protections {first:#x} and \
                     {second:#x}"
                );
                let (pages, page_size) = map_pages(2);
                let boundary = unsafe { pages.add(page_size) };
                fill_boundary(boundary);
                protect(pages, page_size, first);
                protect(boundary, page_size, second);

                let before = successful_gettimeofday();
                let result = raw_gettimeofday(unsafe { boundary.sub(back) }.cast(), unmapped);
                let after = successful_gettimeofday();
                protect(pages, 2 * page_size, read_write);
                let window = boundary_window(boundary, BOUNDARY_WINDOW);

                assert_eq!(result, (-1, libc::EFAULT), "{case}");
                let start = BOUNDARY_WINDOW - back;
                let stored = libc::timeval {
                    tv_sec: window_word(&window, start),
                    tv_usec: window_word(&window, start + 8),
                };
                let offsets = [before, stored, after].map(|tv| timeval_micros(&tv) - epoch_micros);
                assert!(
                    offsets.is_sorted()
                        && offsets[0] >= 0
                        && offsets[2] < MAX_VIRTUAL_OFFSET_MICROS,
                    "{case}: microseconds past the virtual epoch for [ok, write-only tv, ok]: \
                     {offsets:?}"
                );
                let mut expected = vec![BOUNDARY_FILL; window.len()];
                expected[start..start + 16].copy_from_slice(&window[start..start + 16]);
                assert!(
                    window == expected,
                    "{case}: bytes outside tv changed: {window:02x?}"
                );
                assert_eq!(unsafe { libc::munmap(pages.cast(), 2 * page_size) }, 0);
            }
        },
        config,
        true,
    );
}

#[test]
/// A word crossing a page boundary is stored only when both pages are
/// writable. It must stay unchanged when the second page is read-only, even
/// when the first page is write-only and cannot be read back, and when the
/// first page is read-only or inaccessible, even when the second page is
/// write-only and cannot be read back. Twelve bytes before a read-only page,
/// `tv_sec` lies wholly on the write-only page and must be virtual.
fn tod_gettimeofday_faulting_tz_crossing_word_needs_both_pages_writable() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    let epoch_micros = config.epoch.timestamp_micros();
    check_fn_with_config::<Detcore, _>(
        || {
            let unmapped = ptr::without_provenance_mut::<libc::c_void>(1);
            let read_write = libc::PROT_READ | libc::PROT_WRITE;
            // The first page's protection, the second page's, and how many
            // bytes before the boundary `tv` starts.
            for (first, second, back) in [
                (libc::PROT_WRITE, libc::PROT_READ, 4),
                (libc::PROT_WRITE, libc::PROT_READ, 12),
                (libc::PROT_READ, read_write, 4),
                (libc::PROT_NONE, read_write, 4),
                (libc::PROT_READ, libc::PROT_WRITE, 4),
                (libc::PROT_NONE, libc::PROT_WRITE, 4),
            ] {
                let case = format!(
                    "tv {back} bytes before the boundary, page protections {first:#x} and \
                     {second:#x}"
                );
                let (pages, page_size) = map_pages(2);
                let boundary = unsafe { pages.add(page_size) };
                fill_boundary(boundary);
                protect(pages, page_size, first);
                protect(boundary, page_size, second);

                let before = successful_gettimeofday();
                let result = raw_gettimeofday(unsafe { boundary.sub(back) }.cast(), unmapped);
                let after = successful_gettimeofday();
                protect(pages, 2 * page_size, read_write);
                let window = boundary_window(boundary, BOUNDARY_WINDOW);

                assert_eq!(result, (-1, libc::EFAULT), "{case}");
                let offsets = [before, after].map(|tv| timeval_micros(&tv) - epoch_micros);
                assert!(
                    offsets[0] >= 0 && offsets[1] < MAX_VIRTUAL_OFFSET_MICROS,
                    "{case}: microseconds past the virtual epoch for [ok, ok]: {offsets:?}"
                );
                let mut expected = vec![BOUNDARY_FILL; window.len()];
                if back == 12 {
                    let start = BOUNDARY_WINDOW - back;
                    let stored_sec = window_word(&window, start);
                    assert!(
                        (before.tv_sec..=after.tv_sec).contains(&stored_sec),
                        "{case}: tv_sec {stored_sec} is outside the virtual seconds {}..={}",
                        before.tv_sec,
                        after.tv_sec,
                    );
                    expected[start..start + 8].copy_from_slice(&stored_sec.to_ne_bytes());
                }
                assert!(
                    window == expected,
                    "{case}: the bytes from {BOUNDARY_WINDOW} before the boundary are \
                     {window:02x?}, expected {expected:02x?}"
                );
                assert_eq!(unsafe { libc::munmap(pages.cast(), 2 * page_size) }, 0);
            }
        },
        config,
        true,
    );
}

/// Four bytes before the boundary, `tv_sec` starts on a write-only page and
/// ends on an inaccessible or unmapped one. Linux's eight-byte `put_user`
/// stores nothing. Hermit's kernel probe must discover that exact stopping
/// point without modifying either page, and the guest must keep running.
fn unreadable_crossing_tv_sec_stays_unchanged(unmap_second_page: bool) {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    let epoch_micros = config.epoch.timestamp_micros();
    check_fn_with_config::<Detcore, _>(
        move || {
            let unmapped = ptr::without_provenance_mut::<libc::c_void>(1);
            let (pages, page_size) = map_pages(2);
            let boundary = unsafe { pages.add(page_size) };
            fill_boundary(boundary);
            protect(pages, page_size, libc::PROT_WRITE);
            if unmap_second_page {
                assert_eq!(unsafe { libc::munmap(boundary.cast(), page_size) }, 0);
            } else {
                protect(boundary, page_size, libc::PROT_NONE);
            }

            assert_eq!(
                raw_gettimeofday(unsafe { boundary.sub(4) }.cast(), unmapped),
                (-1, libc::EFAULT)
            );
            let after = successful_gettimeofday();
            let offset = timeval_micros(&after) - epoch_micros;
            assert!(
                (0..MAX_VIRTUAL_OFFSET_MICROS).contains(&offset),
                "the guest did not continue with virtual time: {offset} microseconds past epoch"
            );

            protect(pages, page_size, libc::PROT_READ | libc::PROT_WRITE);
            let mapped_after = if unmap_second_page {
                0
            } else {
                protect(boundary, page_size, libc::PROT_READ);
                BOUNDARY_WINDOW
            };
            assert_eq!(
                boundary_window(boundary, mapped_after),
                vec![BOUNDARY_FILL; BOUNDARY_WINDOW + mapped_after],
                "the failed crossing tv_sec must remain byte-for-byte unchanged"
            );
            let mapped_len = if unmap_second_page {
                page_size
            } else {
                2 * page_size
            };
            assert_eq!(unsafe { libc::munmap(pages.cast(), mapped_len) }, 0);
        },
        config,
        true,
    );
}

#[test]
fn tod_gettimeofday_faulting_tz_crossing_into_inaccessible_page_leaves_tv_unchanged() {
    unreadable_crossing_tv_sec_stays_unchanged(false);
}

#[test]
fn tod_gettimeofday_faulting_tz_crossing_into_unmapped_page_leaves_tv_unchanged() {
    unreadable_crossing_tv_sec_stays_unchanged(true);
}

#[test]
/// A protection key disables writes without changing the VMA's ordinary
/// read/write permissions. The kernel probe must honor that thread-local PKRU
/// state and therefore leave `tv` untouched when `tz` also faults.
fn tod_gettimeofday_faulting_tz_pkey_write_disabled_leaves_tv_unchanged() {
    const PKEY_DISABLE_WRITE: libc::c_ulong = 0x2;

    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    let epoch_micros = config.epoch.timestamp_micros();
    check_fn_with_config::<Detcore, _>(
        move || {
            let (page, page_size) = map_pages(1);
            let tv = page.cast::<libc::timeval>();
            unsafe { tv.write(SENTINEL_TV) };

            let pkey = unsafe { libc::syscall(libc::SYS_pkey_alloc, 0, PKEY_DISABLE_WRITE) };
            assert_ne!(
                pkey,
                -1,
                "host lacks the pkey_alloc/PKEY_DISABLE_WRITE capability: {}",
                std::io::Error::last_os_error()
            );
            let assigned = unsafe {
                libc::syscall(
                    libc::SYS_pkey_mprotect,
                    page,
                    page_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    pkey,
                )
            };
            assert_eq!(
                assigned,
                0,
                "pkey_mprotect could not assign key {pkey}: {}",
                std::io::Error::last_os_error()
            );

            let unmapped = ptr::without_provenance_mut::<libc::c_void>(1);
            assert_eq!(raw_gettimeofday(tv, unmapped), (-1, libc::EFAULT));
            let after = successful_gettimeofday();
            let offset = timeval_micros(&after) - epoch_micros;
            assert!(
                (0..MAX_VIRTUAL_OFFSET_MICROS).contains(&offset),
                "the guest did not continue with virtual time: {offset} microseconds past epoch"
            );

            let restored = unsafe {
                libc::syscall(
                    libc::SYS_pkey_mprotect,
                    page,
                    page_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    0,
                )
            };
            assert_eq!(
                restored,
                0,
                "pkey_mprotect could not restore key 0: {}",
                std::io::Error::last_os_error()
            );
            let observed = unsafe { tv.read() };
            assert_eq!(
                (observed.tv_sec, observed.tv_usec),
                (SENTINEL_TV.tv_sec, SENTINEL_TV.tv_usec),
                "the PKEY_DISABLE_WRITE timeval must remain byte-for-byte unchanged"
            );
            assert_eq!(
                unsafe { libc::syscall(libc::SYS_pkey_free, pkey) },
                0,
                "pkey_free({pkey}) failed: {}",
                std::io::Error::last_os_error()
            );
            assert_eq!(unsafe { libc::munmap(page.cast(), page_size) }, 0);
        },
        config,
        true,
    );
}

#[test]
/// With no `tv`, a faulting `tz` still fails with EFAULT.
fn tod_gettimeofday_null_tv_faulting_tz_fails() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    check_fn_with_config::<Detcore, _>(
        || {
            let unmapped = ptr::without_provenance_mut::<libc::c_void>(1);
            assert_eq!(
                raw_gettimeofday(ptr::null_mut(), unmapped),
                (-1, libc::EFAULT)
            );
        },
        config,
        true,
    );
}

/// Installs a seccomp filter in the calling process that makes `time(2)` fail
/// with EFAULT without running it: every call, or with `only_tloc`, only a
/// call whose `tloc` is that address. It also makes `ptrace(PTRACE_PEEKDATA)`
/// at each address in `deny_peekdata_at` fail with EPERM, so a tracer in this
/// process cannot read those guest words.
fn install_time_efault_filter(only_tloc: Option<usize>, deny_peekdata_at: &[usize]) {
    const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
    // Offsets of `nr`, `arch` and the two halves of `args[0]` and `args[2]`
    // in `struct seccomp_data`.
    const NR: u32 = 0;
    const ARCH: u32 = 4;
    const ARG0_LO: u32 = 16;
    const ARG0_HI: u32 = 20;
    // `ptrace(request, pid, addr, data)`: `addr` is `args[2]`.
    const ARG2_LO: u32 = 32;
    const ARG2_HI: u32 = 36;
    let stmt = |code: u32, k: u32| libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    };
    let jeq = |k: u32, jt: u8, jf: u8| libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt,
        jf,
        k,
    };
    let load = |offset| stmt(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, offset);
    let allow = stmt(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW);
    let efault = stmt(
        libc::BPF_RET | libc::BPF_K,
        libc::SECCOMP_RET_ERRNO | libc::EFAULT as u32,
    );
    let eperm = stmt(
        libc::BPF_RET | libc::BPF_K,
        libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
    );
    let mut filter = vec![load(ARCH), jeq(AUDIT_ARCH_X86_64, 1, 0), allow, load(NR)];
    if !deny_peekdata_at.is_empty() {
        let per_addr = 5 * deny_peekdata_at.len() as u8;
        filter.extend([
            jeq(libc::SYS_ptrace as u32, 0, 2 + per_addr),
            load(ARG0_LO),
            jeq(libc::PTRACE_PEEKDATA, 0, per_addr),
        ]);
        for &addr in deny_peekdata_at {
            filter.extend([
                load(ARG2_LO),
                jeq(addr as u32, 0, 3),
                load(ARG2_HI),
                jeq((addr >> 32) as u32, 0, 1),
                eperm,
            ]);
        }
        filter.push(load(NR));
    }
    match only_tloc {
        None => filter.extend([jeq(libc::SYS_time as u32, 0, 1), efault, allow]),
        Some(tloc) => filter.extend([
            jeq(libc::SYS_time as u32, 0, 5),
            load(ARG0_LO),
            jeq(tloc as u32, 0, 3),
            load(ARG0_HI),
            jeq((tloc >> 32) as u32, 0, 1),
            efault,
            allow,
        ]),
    }
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        0
    );
    assert_eq!(
        unsafe {
            libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &prog as *const libc::sock_fprog,
            )
        },
        0
    );
}

const RESUMED_AFTER_FAULT: &str = "resumed-after-faulting-gettimeofday";

/// A seccomp filter inherited from Hermit's parent that returns EFAULT for
/// `time(2)` makes Detcore's `time(2)` store probe report EFAULT although
/// nothing faulted, while the host has already stored its wall clock in a
/// writable `tv`. The run must stop before the guest resumes and can read that
/// host time. Detcore emulates the guest's own `seccomp(2)` and refuses
/// `PR_SET_NO_NEW_PRIVS`, so the filter is installed on this test thread, which
/// forks the guest and so passes the filter on to it.
///
/// With `deny_peekdata`, the filter also stops the tracer, which runs on this
/// thread, from reading `tv`, so the pre-call snapshot and the post-probe read
/// both fail although `tv` is mapped and the host stored its time there.
fn seccomp_efault_time_probe_stops_the_run(
    only_probe_address: bool,
    deny_peekdata: bool,
    expected: &str,
) {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    // Mapped before the fork, so the guest has it at the same address.
    let (page, _) = map_pages(1);
    let tv_raw = page.expose_provenance();
    let tv_words = [
        tv_raw,
        tv_raw + std::mem::offset_of!(libc::timeval, tv_usec),
    ];
    let deny_peekdata_at: &[usize] = if deny_peekdata { &tv_words } else { &[] };
    install_time_efault_filter(only_probe_address.then_some(tv_raw), deny_peekdata_at);
    let outcome = test_fn_with_config::<Detcore, _>(
        move || {
            let tv_addr = ptr::with_exposed_provenance_mut::<libc::timeval>(tv_raw);
            let unmapped = ptr::without_provenance_mut::<libc::c_void>(1);
            unsafe { tv_addr.write(SENTINEL_TV) };
            // The filter is in force in the guest: its own call fails without
            // storing anything.
            let probe = if only_probe_address {
                tv_addr.cast::<libc::time_t>()
            } else {
                ptr::null_mut()
            };
            let ret = unsafe { libc::syscall(libc::SYS_time, probe) };
            assert_eq!(
                (ret, unsafe { *libc::__errno_location() }),
                (-1, libc::EFAULT)
            );
            assert_eq!(unsafe { (*tv_addr).tv_sec }, SENTINEL_TV.tv_sec);

            let result = raw_gettimeofday(tv_addr, unmapped);
            println!("{RESUMED_AFTER_FAULT} {result:?} tv_sec={}", unsafe {
                (*tv_addr).tv_sec
            });
        },
        config,
        true,
    );
    let error = match outcome {
        Err(error) => error,
        Ok((output, _)) => panic!(
            "the run finished with {:?}; stdout: {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    };
    let message = format!("{error:#}");
    assert!(message.contains(expected), "unexpected failure: {message}");
}

#[test]
fn tod_gettimeofday_faulting_tz_seccomp_efault_for_every_time_call_stops_the_run() {
    seccomp_efault_time_probe_stops_the_run(false, false, "time(NULL) control probe failed");
}

#[test]
fn tod_gettimeofday_faulting_tz_seccomp_efault_for_the_probe_address_stops_the_run() {
    seccomp_efault_time_probe_stops_the_run(true, false, "the word changed during the call");
}

#[test]
fn tod_gettimeofday_faulting_tz_seccomp_efault_with_unreadable_mapped_tv_stops_the_run() {
    seccomp_efault_time_probe_stops_the_run(true, true, "the word is mapped but could not be read");
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
            let delta_ms = diff_millis(epoch, dt);
            // However exactly we compute logical time, this should be within a small
            // fraction of a (logical) second of epoch:
            assert!(
                (0..100).contains(&delta_ms),
                "clock_gettime time {dt} is not within 100 ms after epoch {epoch}: difference {delta_ms} ms"
            );
        },
        config,
        true,
    );
}

#[cfg_attr(
    fbcode_build,
    ignore = "fbsource-disabled: hangs or crashes at exit under v0.4.1; reworked upstream after v0.4.1 and re-enabled by the 0.5 import (D125030699)"
)]
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

#[cfg_attr(
    fbcode_build,
    ignore = "fbsource-disabled: hangs or crashes at exit under v0.4.1; reworked upstream after v0.4.1 and re-enabled by the 0.5 import (D125030699)"
)]
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

#[cfg_attr(
    fbcode_build,
    ignore = "fbsource-disabled: hangs or crashes at exit under v0.4.1; reworked upstream after v0.4.1 and re-enabled by the 0.5 import (D125030699)"
)]
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
