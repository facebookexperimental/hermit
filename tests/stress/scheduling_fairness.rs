/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::VecDeque;
use std::env;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;

const THREADS: usize = 4;
const COUNTER_TURNS: usize = 64;
const QUEUE_CAPACITY: usize = 8;
const QUEUE_ITEMS: usize = 256;
const WRITER_ROUNDS: usize = 32;

fn counter_fairness() {
    let start = Arc::new(Barrier::new(THREADS));
    let started = Arc::new(AtomicUsize::new(0));
    let progress = Arc::new(AtomicUsize::new(0));
    let counts = Arc::new(
        (0..THREADS)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );
    let last_progress = Arc::new(
        (0..THREADS)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );
    let max_gaps = Arc::new(
        (0..THREADS)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );

    let handles = (0..THREADS)
        .map(|id| {
            let start = Arc::clone(&start);
            let started = Arc::clone(&started);
            let progress = Arc::clone(&progress);
            let counts = Arc::clone(&counts);
            let last_progress = Arc::clone(&last_progress);
            let max_gaps = Arc::clone(&max_gaps);
            thread::spawn(move || {
                start.wait();
                started.fetch_add(1, Ordering::SeqCst);
                while started.load(Ordering::SeqCst) != THREADS {
                    thread::yield_now();
                }

                for _ in 0..COUNTER_TURNS {
                    let current = progress.fetch_add(1, Ordering::SeqCst) + 1;
                    let previous = last_progress[id].swap(current, Ordering::SeqCst);
                    if previous != 0 {
                        max_gaps[id].fetch_max(current - previous - 1, Ordering::SeqCst);
                    }
                    counts[id].fetch_add(1, Ordering::SeqCst);
                    thread::yield_now();
                }
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.join().unwrap();
    }

    let counts = counts
        .iter()
        .map(|count| count.load(Ordering::SeqCst))
        .collect::<Vec<_>>();
    let max_gaps = max_gaps
        .iter()
        .map(|gap| gap.load(Ordering::SeqCst))
        .collect::<Vec<_>>();
    let worst = max_gaps.iter().copied().max().unwrap();
    println!(
        "counter counts={} max_gaps={} worst={worst}",
        comma_separated(&counts),
        comma_separated(&max_gaps)
    );
}

#[derive(Default)]
struct QueueState {
    items: VecDeque<usize>,
    produced: usize,
    consumed: usize,
    consumer_streak: usize,
    max_consumer_streak: usize,
    done: bool,
}

struct BoundedQueue {
    state: Mutex<QueueState>,
    not_empty: Condvar,
    not_full: Condvar,
}

impl BoundedQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
        }
    }
}

fn producer_consumer_fairness() {
    let queue = Arc::new(BoundedQueue::new());
    let start = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::new();

    let producer_queue = Arc::clone(&queue);
    let producer_start = Arc::clone(&start);
    handles.push(thread::spawn(move || {
        producer_start.wait();
        for item in 0..QUEUE_ITEMS {
            let mut state = producer_queue.state.lock().unwrap();
            while state.items.len() == QUEUE_CAPACITY {
                state = producer_queue.not_full.wait(state).unwrap();
            }
            state.items.push_back(item);
            state.produced += 1;
            state.consumer_streak = 0;
            producer_queue.not_empty.notify_one();
            drop(state);
            thread::yield_now();
        }

        let mut state = producer_queue.state.lock().unwrap();
        state.done = true;
        producer_queue.not_empty.notify_all();
    }));

    for _ in 1..THREADS {
        let consumer_queue = Arc::clone(&queue);
        let consumer_start = Arc::clone(&start);
        handles.push(thread::spawn(move || {
            consumer_start.wait();
            loop {
                let mut state = consumer_queue.state.lock().unwrap();
                while state.items.is_empty() && !state.done {
                    state = consumer_queue.not_empty.wait(state).unwrap();
                }
                if state.items.pop_front().is_none() {
                    assert!(state.done);
                    break;
                }
                state.consumed += 1;
                state.consumer_streak += 1;
                state.max_consumer_streak = state.max_consumer_streak.max(state.consumer_streak);
                consumer_queue.not_full.notify_one();
                drop(state);
                thread::yield_now();
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let state = queue.state.lock().unwrap();
    println!(
        "producer_consumer produced={} consumed={} max_consumer_streak={} capacity={QUEUE_CAPACITY}",
        state.produced, state.consumed, state.max_consumer_streak
    );
}

fn rwlock_fairness() {
    let lock = Arc::new(RwLock::new(0usize));
    let start = Arc::new(Barrier::new(THREADS));
    let writer_waiting = Arc::new(AtomicBool::new(false));
    let reads_while_waiting = Arc::new(AtomicUsize::new(0));
    let max_reads_while_waiting = Arc::new(AtomicUsize::new(0));
    let total_reads = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();

    for _ in 1..THREADS {
        let reader_lock = Arc::clone(&lock);
        let reader_start = Arc::clone(&start);
        let reader_writer_waiting = Arc::clone(&writer_waiting);
        let reader_reads_while_waiting = Arc::clone(&reads_while_waiting);
        let reader_total_reads = Arc::clone(&total_reads);
        let reader_done = Arc::clone(&done);
        handles.push(thread::spawn(move || {
            reader_start.wait();
            while !reader_done.load(Ordering::SeqCst) {
                let value = reader_lock.read().unwrap();
                std::hint::black_box(*value);
                if reader_writer_waiting.load(Ordering::SeqCst) {
                    reader_reads_while_waiting.fetch_add(1, Ordering::SeqCst);
                }
                reader_total_reads.fetch_add(1, Ordering::SeqCst);
                drop(value);
                thread::yield_now();
            }
        }));
    }

    let writer_lock = Arc::clone(&lock);
    let writer_start = Arc::clone(&start);
    let writer_waiting_flag = Arc::clone(&writer_waiting);
    let writer_reads_while_waiting = Arc::clone(&reads_while_waiting);
    let writer_max_reads_while_waiting = Arc::clone(&max_reads_while_waiting);
    let writer_done = Arc::clone(&done);
    handles.push(thread::spawn(move || {
        writer_start.wait();
        for _ in 0..WRITER_ROUNDS {
            writer_reads_while_waiting.store(0, Ordering::SeqCst);
            writer_waiting_flag.store(true, Ordering::SeqCst);
            let mut value = writer_lock.write().unwrap();
            writer_waiting_flag.store(false, Ordering::SeqCst);
            writer_max_reads_while_waiting.fetch_max(
                writer_reads_while_waiting.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
            *value += 1;
            drop(value);
            thread::yield_now();
        }
        writer_done.store(true, Ordering::SeqCst);
    }));

    for handle in handles {
        handle.join().unwrap();
    }

    println!(
        "rwlock writes={} reads={} max_reads_while_writer_waiting={}",
        *lock.read().unwrap(),
        total_reads.load(Ordering::SeqCst),
        max_reads_while_waiting.load(Ordering::SeqCst)
    );
}

fn comma_separated(values: &[usize]) -> String {
    values
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn main() {
    let workload = env::args().nth(1).expect("fairness workload is required");
    match workload.as_str() {
        "counter" => counter_fairness(),
        "producer-consumer" => producer_consumer_fairness(),
        "rwlock" => rwlock_fairness(),
        _ => panic!("unknown fairness workload: {workload}"),
    }
}
