/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#![feature(get_mut_unchecked)]
#![feature(thread_id_value)]
#![feature(atomic_mut_ptr)]

use std::sync::Arc;

#[global_allocator]
static ALLOC: test_allocator::Global = test_allocator::Global;

/// Calculate the number of switch points. E.g. the number of times we observed interleaved
/// writes between the threads.
fn count_switch_points(shared_data: &Arc<[u64]>) -> u64 {
    // Calculate the number of switch points. E.g. the number of times we observed interleaved
    // writes between the threads.
    let mut switch_points = 0;
    let mut prev = shared_data[0];
    for i in 1..shared_data.len() {
        if prev != shared_data[i] {
            prev = shared_data[i];
            switch_points += 1;
        }
    }
    switch_points
}

/// Race on memory access.
mod mem_race {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::thread;

    use detcore_testutils::det_test_fn_with_config;
    use detcore_testutils::expect_success;
    use reverie_ptrace::testing::check_fn_with_config;
    const NUM_ELEMENTS: usize = 20_000_000;

    /// In guest mode two threads will try to fill up half of the data array with their thread id as
    /// value. The threads grab indices through an atomic int. For sufficiently large arrays we expect
    /// the thread ids to show up interleaved.
    fn raw() -> u64 {
        let shared_data: Arc<[u64]> = vec![0; NUM_ELEMENTS].into();
        let shared_idx = Arc::new(AtomicUsize::new(0));

        fn worker(idx: Arc<AtomicUsize>, mut data: Arc<[u64]>) {
            let tid = thread::current().id().as_u64().get();
            // Get a mutable reference to the data. This is unsafe, but we guarantee the
            // threads are always accesssing unique non-overlapping indices of the array.
            let data = unsafe { Arc::get_mut_unchecked(&mut data) };

            // Give each thread half of the fetch_add attempts.
            for _ in 0..(NUM_ELEMENTS / 2) {
                let idx = idx.fetch_add(1, Ordering::SeqCst);
                data[idx] = tid;
            }
        }

        // One-armed variant: spawn one child:
        let handle = {
            let (idx, data) = (shared_idx.clone(), shared_data.clone());
            thread::spawn(move || {
                // This exercises the futex_wait-called-by-kernel behavior, even on "bottom":
                worker(idx, data)
            })
        };
        println!("Parent done spawning child thread and starting own work...");
        worker(shared_idx, shared_data.clone());

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
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::thread;

    use detcore::Detcore;
    use pretty_assertions::assert_eq;
    use reverie::ExitStatus;
    use reverie_ptrace::testing::test_fn_with_config;
    const NUM_ELEMENTS: usize = 50_000_000;
    const CHUNKS: usize = 5;

    fn raw() -> u64 {
        let shared_data: Arc<[u64]> = vec![0; NUM_ELEMENTS].into();
        let shared_idx = Arc::new(AtomicUsize::new(0));

        fn worker(idx: Arc<AtomicUsize>, mut data: Arc<[u64]>, rank: usize) {
            let tid = thread::current().id().as_u64().get();
            let data = unsafe { Arc::get_mut_unchecked(&mut data) };

            for _i in 0..CHUNKS {
                let s = format!("{} ", rank);
                eprint!("{}", s);
                for _ in 0..(NUM_ELEMENTS / 2 / CHUNKS) {
                    let idx = idx.fetch_add(1, Ordering::SeqCst);
                    data[idx] = tid;
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
        // In raw executions, these will typically be around 900K switches:
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
            // In bottom/middle modes, this will typically do about 150K switches:
            test_fn_with_config::<Detcore, _>(raw_assert(1), cfg, false).unwrap()
        };
        reverie_ptrace::testing::print_tracee_output(&output);
        assert_eq!(output.status, ExitStatus::Exited(0));
    }
}

/// Parent thread waits on futex
mod futex_wait_parent {
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
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
