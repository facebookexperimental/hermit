// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

#![feature(get_mut_unchecked)]
#![feature(thread_id_value)]
#![feature(atomic_mut_ptr)]

use std::env;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
const NUM_ELEMENTS: usize = 20_000_000;

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

/// In guest mode two threads will try to fill up half of the data array with their thread id as
/// value. The threads grab indices through an atomic int. For sufficiently large arrays we expect
/// the thread ids to show up interleaved.
fn run_test() -> u64 {
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
