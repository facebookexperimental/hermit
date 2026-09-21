/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#![allow(
    unexpected_cfgs,
    reason = "`sanitized` is supplied by the internal sanitizer build"
)]

use std::cell::UnsafeCell;
use std::sync::Arc;

#[global_allocator]
static ALLOC: test_allocator::Global = test_allocator::Global;

/// A shared array the workers mutate concurrently through `&self`.
///
/// WHY `UnsafeCell` AND NOT A RAW POINTER. `Arc` hands out shared access only, so
/// a pointer derived from it carries read-only provenance and no cast adds write
/// provenance to it -- that is a language rule, not a lint, and miri rejects the
/// write. `UnsafeCell` is the sanctioned way to mutate through a shared
/// reference. Two earlier spellings of these loops were both unsound:
///
/// ```text
/// Arc::get_mut_unchecked(&mut data)                   UB: data race between the
///                                                         non-atomic write and the
///                                                         other thread's retag
/// from_raw_parts_mut(data.as_ptr() as *mut u64, len)  UB: retag for Unique from a
///                                                         tag granting only
///                                                         SharedReadOnly
/// ```
///
/// WHY `Arc<[UnsafeCell<u64>]>` AND NOT `Arc<Vec<UnsafeCell<u64>>>`. The elements
/// of an `Arc<[T]>` live inside the `Arc` allocation, so reaching one is a single
/// indirection -- exactly what `Arc<[u64]>` cost before. An `Arc<Vec<T>>` stores a
/// `Vec` header in that slot and the elements elsewhere, adding a load to a loop
/// that runs millions of times per thread. These arrays are the subject of the
/// weekly `mem_race` PMU gates, whose premise is that preemption is driven by
/// retired conditional branches, so an extra load or a dropped bounds check moves
/// the numbers they pin.
pub(crate) struct SharedCells(Arc<[UnsafeCell<u64>]>);

// SAFETY: the workers only ever write cells whose index came from the shared
// atomic counter, so no two threads touch the same cell, and every read happens
// after `join()`. Sharing the handle is therefore sound; the type cannot enforce
// that discipline, which is why this impl is unsafe and why the invariant is
// stated here rather than assumed.
unsafe impl Send for SharedCells {}
unsafe impl Sync for SharedCells {}

impl Clone for SharedCells {
    fn clone(&self) -> Self {
        SharedCells(Arc::clone(&self.0))
    }
}

impl SharedCells {
    pub(crate) fn zeroed(len: usize) -> Self {
        let cells: Vec<UnsafeCell<u64>> = (0..len).map(|_| UnsafeCell::new(0)).collect();
        SharedCells(cells.into())
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Read a cell. Callers must have established happens-before with the writers.
    pub(crate) fn get(&self, i: usize) -> u64 {
        // SAFETY: indexing keeps its bounds check; the caller has joined the workers.
        unsafe { *self.0[i].get() }
    }

    /// Write a cell. Callers must own `i` exclusively.
    pub(crate) fn set(&self, i: usize, v: u64) {
        // SAFETY: `i` came from the shared counter, so this worker owns it. `self.0[i]`
        // is a bounds-checked index -- do NOT replace it with `get_unchecked`, the
        // branch it emits is part of what the RCB preemption timer measures.
        unsafe { *self.0[i].get() = v }
    }
}

/// Calculate the number of switch points. E.g. the number of times we observed interleaved
/// writes between the threads.
fn count_switch_points(shared_data: &SharedCells) -> u64 {
    // Calculate the number of switch points. E.g. the number of times we observed interleaved
    // writes between the threads.
    let mut switch_points = 0;
    let mut prev = shared_data.get(0);
    for i in 1..shared_data.len() {
        if prev != shared_data.get(i) {
            prev = shared_data.get(i);
            switch_points += 1;
        }
    }
    switch_points
}

/// Race on memory access.
mod mem_race {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::thread;

    use detcore_testutils::check_fn_with_config;
    use detcore_testutils::det_test_fn_with_config;
    use detcore_testutils::expect_success;
    const NUM_ELEMENTS: usize = 20_000_000;

    /// In guest mode two threads will try to fill up half of the data array with their thread id as
    /// value. The threads grab indices through an atomic int. For sufficiently large arrays we expect
    /// the thread ids to show up interleaved.
    fn raw() -> u64 {
        let shared_data = super::SharedCells::zeroed(NUM_ELEMENTS);
        let shared_idx = Arc::new(AtomicUsize::new(0));

        // `tag` is the distinct per-worker value written into the array. It used to
        // be the thread id, but `ThreadId` has no stable numeric accessor and the
        // test only ever needs the two workers to write DIFFERENT values --
        // `count_switch_points` compares adjacent elements and never inspects the
        // value itself. They must nevertheless stay DISTINCT and nonzero: give both
        // workers the same tag and the switch-point count collapses to zero, which
        // is the test measuring nothing while still compiling and running.
        fn worker(idx: Arc<AtomicUsize>, data: super::SharedCells, tag: u64) {
            // The contention is the point of this test and must survive any tidy-up:
            // two threads interleaving writes into ONE array, ordered only by the
            // shared counter, is what produces switch points at all. What must NOT
            // survive is undefined behaviour -- the elements never raced (the counter
            // hands out disjoint indices); what raced was the construction of the
            // `&mut`. So do NOT "fix" this with atomics: that changes the memory
            // operations the test exists to exercise, and it is not what was wrong.
            // Give each thread half of the fetch_add attempts.
            for _ in 0..(NUM_ELEMENTS / 2) {
                let idx = idx.fetch_add(1, Ordering::SeqCst);
                data.set(idx, tag);
            }
        }

        // One-armed variant: spawn one child:
        let handle = {
            let (idx, data) = (shared_idx.clone(), shared_data.clone());
            thread::spawn(move || {
                // This exercises the futex_wait-called-by-kernel behavior, even on "bottom":
                worker(idx, data, 1)
            })
        };
        println!("Parent done spawning child thread and starting own work...");
        worker(shared_idx, shared_data.clone(), 2);

        println!("Parent done with work and joining child thread..");
        handle.join().unwrap();

        let switch_points = super::count_switch_points(&shared_data);
        let s: String = format!("Switch points: {}\n", switch_points);
        println!("{}", s); // Print a bit more atomically.
        // Only when running deterministically:
        switch_points
    }

    #[test] // Optional: raw, uninstrumented test.
    fn raw_run_par_mode() {
        eprintln!("Running in parallel, expecting interleavings.");
        // Intentionally racy but with (arbitrarily) low probability of failure:
        if raw() <= 1 {
            eprintln!("Expected more than 1 switch point!");
            std::process::exit(99);
        }
    }

    // Running under Reverie, there SHOULD be interleavings, but we can't 100% count on it.
    fn raw_run_par_mode_reverie() {
        if raw() <= 1 {
            eprintln!(
                "Slightly surprising that there's only 1 switch point under Reverie! But whatever."
            );
        }
    }

    // An additional test under a non-Detcore Reverie tool:
    #[allow(dead_code)]
    fn run_noop_mode() {
        check_fn_with_config::<(), _>(raw_run_par_mode_reverie, (), false);
    }

    #[test]
    #[cfg(not(sanitized))]
    fn noop_mode() {
        run_noop_mode();
    }

    #[cfg(not(sanitized))]
    detcore_testutils::make_det_test_variants!(detcore, "all");

    /// Two threads print different characters, interleaved.
    #[allow(dead_code)]
    pub fn detcore(cfg: &detcore::Config) {
        fn run_seq_mode() {
            eprintln!("Running sequentialized, deterministically.");
            let switches = raw();
            assert!(
                switches > 10,
                "Expecting deterministic preemptions when using RCB timers"
            );
        }
        let cfg = cfg.clone();
        if cfg.sequentialize_threads {
            det_test_fn_with_config(true, run_seq_mode, cfg, expect_success);
        } else {
            det_test_fn_with_config(false, raw_run_par_mode_reverie, cfg, expect_success);
        }
    }

    #[test]
    #[cfg(not(sanitized))]
    pub fn with_signal() {}
}

/// Race both memory ops and prints.
mod mem_print_race {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::thread;

    use detcore::Detcore;
    use detcore_testutils::test_fn_with_config;
    use pretty_assertions::assert_eq;
    use reverie::ExitStatus;
    // Keep enough atomic operations to observe robust interleaving without making
    // the ptrace variants spend minutes exercising the same scheduling path.
    const NUM_ELEMENTS: usize = 2_000_000;
    const CHUNKS: usize = 5;

    fn raw() -> u64 {
        let shared_data = super::SharedCells::zeroed(NUM_ELEMENTS);
        let shared_idx = Arc::new(AtomicUsize::new(0));

        fn worker(idx: Arc<AtomicUsize>, data: super::SharedCells, rank: usize) {
            // Distinct nonzero value per worker; see the note in mem_race::worker.
            let tid = rank as u64 + 1;
            // As in mem_race::worker: disjoint indices from the shared counter, the
            // contention is deliberate, and the bounds check is load-bearing for the
            // RCB profile. Do NOT "fix" it with atomics.
            for _i in 0..CHUNKS {
                let s = format!("{} ", rank);
                eprint!("{}", s);
                for _ in 0..(NUM_ELEMENTS / 2 / CHUNKS) {
                    let idx = idx.fetch_add(1, Ordering::SeqCst);
                    data.set(idx, tid);
                }
            }
            std::io::Write::flush(&mut std::io::stderr()).unwrap();
        }

        let handle = {
            let (idx, data) = (shared_idx.clone(), shared_data.clone());
            thread::spawn(move || worker(idx, data, 0))
        };
        worker(shared_idx, shared_data.clone(), 1);
        handle.join().unwrap();

        let switch_points = super::count_switch_points(&shared_data);
        let s: String = format!("\nSwitch points: {}\n", switch_points);
        println!("{}", s);
        switch_points
    }

    fn raw_assert(thresh: u64) -> impl Fn() {
        move || {
            eprintln!("Running in parallel, expecting interleavings.");
            if raw() <= thresh {
                eprintln!("Expected more than {} switch point(s)!", thresh);
                std::process::exit(1);
            }
        }
    }

    #[test]
    fn raw_run_par_mode() {
        // Raw executions should still produce many more switches than this minimum.
        raw_assert(1)();
    }

    #[cfg(not(sanitized))]
    detcore_testutils::make_det_test_variants!(detcore, "all");

    /// Two threads print different characters, interleaved.
    #[allow(dead_code)]
    pub fn detcore(cfg: &detcore::Config) {
        eprintln!("Running detcore test with {} chunks", CHUNKS);
        let cfg = cfg.clone();
        let (output, _state) = if cfg.sequentialize_threads {
            // Due to fair round-robin scheduling we interleave on almost every write.
            // There are some boundary conditions that prevent this from hitting exactly 2*CHUNKS:
            test_fn_with_config::<Detcore, _>(raw_assert(2 * CHUNKS as u64 - 10), cfg, false)
                .unwrap()
        } else {
            // Bottom/middle modes run concurrently and should interleave naturally.
            test_fn_with_config::<Detcore, _>(raw_assert(1), cfg, false).unwrap()
        };
        reverie_ptrace::testing::print_tracee_output(&output);
        assert_eq!(output.status, ExitStatus::Exited(0));
    }
}

/// Parent thread waits on futex
mod futex_wait_parent {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    use std::thread;

    #[cfg(not(sanitized))]
    // TODO: currently "top" mode dies in the assertion at the end of step5:
    detcore_testutils::basic_det_test!(
        raw,
        |c: &detcore::Config| c.sequentialize_threads,
        "bottom",
        "middle",
        "default"
    );

    #[test]
    fn raw() {
        let sem = Arc::new(AtomicU32::new(1000));
        let ptr = sem.as_ptr();
        let ptr2 = ptr as usize;
        let sem2 = sem.clone();

        eprintln!("Parent thread: spawn child.");
        let _ = thread::spawn(move || {
            let ptr: *mut u8 = ptr2 as *mut u8;
            eprintln!("Child thread start.");
            std::thread::sleep(std::time::Duration::from_millis(500));
            sem2.fetch_add(1, Ordering::SeqCst);
            eprintln!("Child thread: start futex wake ({:?})..", ptr);
            let res = unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    ptr,
                    libc::FUTEX_WAKE,
                    1000, // val: wake 1 thread
                    0,    // timeout - ignored
                    0,    // uaddr - ignored
                    0,    // val3 - ignored
                )
            };
            let s = format!("Child thread: futex wake returned {}, done.\n", res);
            eprint!("{}", s);
        });
        eprintln!("Parent thread: futex wait ({:?})..", ptr);
        let observation = sem.load(Ordering::SeqCst);
        let res = unsafe {
            libc::syscall(
                libc::SYS_futex,
                ptr,
                libc::FUTEX_WAIT,
                1000, // val,
                0,    // timeout,
                0,    // uaddr - ignored
                0,    // val3 - ignored
            )
        };
        std::thread::sleep(std::time::Duration::from_millis(500));
        let s = format!(
            "Parent thread: done with futex wait, rax: {}, observation {}\n",
            res, observation
        );
        eprint!("{}", s);
        std::thread::sleep(std::time::Duration::from_millis(500));
        eprintln!("Parent thread: exiting process.");
        let _ = unsafe { libc::syscall(libc::SYS_exit_group, 0) };
    }
}
