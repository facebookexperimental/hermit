/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cell::UnsafeCell;
use std::env;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
const NUM_ELEMENTS: usize = 20_000_000;

/// A shared array the workers mutate concurrently through `&self`.
///
/// WHY `UnsafeCell` AND NOT A RAW POINTER. `Arc` hands out shared access only, so
/// a pointer derived from it carries read-only provenance and no cast adds write
/// provenance to it -- that is a language rule, not a lint, and miri rejects the
/// write. `UnsafeCell` is the sanctioned way to mutate through a shared
/// reference. Two earlier spellings of this loop were both unsound:
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
/// that runs ten million times per thread. This array is the subject of the weekly
/// `mem_race` PMU gates, whose premise is that preemption is driven by retired
/// conditional branches, so an extra load or a dropped bounds check moves the
/// numbers they pin.
struct SharedCells(Arc<[UnsafeCell<u64>]>);

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
    fn zeroed(len: usize) -> Self {
        let cells: Vec<UnsafeCell<u64>> = (0..len).map(|_| UnsafeCell::new(0)).collect();
        SharedCells(cells.into())
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    /// Read a cell. Callers must have established happens-before with the writers.
    fn get(&self, i: usize) -> u64 {
        // SAFETY: indexing keeps its bounds check; the caller has joined the workers.
        unsafe { *self.0[i].get() }
    }

    /// Write a cell. Callers must own `i` exclusively.
    fn set(&self, i: usize, v: u64) {
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

/// In guest mode two threads will try to fill up half of the data array with their thread id as
/// value. The threads grab indices through an atomic int. For sufficiently large arrays we expect
/// the thread ids to show up interleaved.
fn run_test() -> u64 {
    let shared_data = SharedCells::zeroed(NUM_ELEMENTS);
    let shared_idx = Arc::new(AtomicUsize::new(0));

    // `tag` is the distinct per-worker value written into the array. It used to be
    // the thread id, but `ThreadId` has no stable numeric accessor and the test only
    // needs the two workers to write DIFFERENT values -- count_switch_points compares
    // adjacent elements and never inspects the value itself. They must nevertheless
    // stay DISTINCT and nonzero: give both workers the same tag and the switch-point
    // count collapses to zero, which is the test measuring nothing while still
    // passing its compile.
    fn worker(idx: Arc<AtomicUsize>, data: SharedCells, tag: u64) {
        // The contention is the point of this test and must survive any tidy-up: two
        // threads interleaving writes into ONE array, ordered only by the shared
        // counter, is what produces switch points at all. What must NOT survive is
        // undefined behaviour -- the elements never raced (the counter hands out
        // disjoint indices); what raced was the construction of the `&mut`. So do NOT
        // "fix" this with atomics: that changes the memory operations the test exists
        // to exercise, and it is not what was ever wrong here.
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

    let switch_points = count_switch_points(&shared_data);
    let s: String = format!("Switch points: {}\n", switch_points);
    println!("{}", s); // Print a bit more atomically.
    // Only when running deterministically:
    switch_points
}

fn main() {
    if env::var("HERMIT_MODE") == Ok("strict".to_string()) {
        eprintln!("Running sequentialized, deterministically.");
        let switches = run_test();
        assert!(
            switches > 1,
            "Expecting deterministic preemptions when using RCB timers"
        );
    } else {
        eprintln!("Running mem_race, but not expecting determinism.");
        let _switches = run_test();
    }
}
