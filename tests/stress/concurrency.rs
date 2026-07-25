/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::VecDeque;
use std::env;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;

const CONTENTION_ROUNDS: usize = 50;
const RWLOCK_ROUNDS: usize = 100;
const QUEUE_ITEMS: usize = 64;

fn atomic_lost_update(threads: usize) -> bool {
    let value = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let value = Arc::clone(&value);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                let current = value.load(Ordering::SeqCst);
                thread::yield_now();
                value.store(current + 1, Ordering::SeqCst);
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }
    value.load(Ordering::SeqCst) != threads
}

fn publish_ordering(threads: usize) -> bool {
    let data = Arc::new(AtomicUsize::new(0));
    let published = Arc::new(AtomicBool::new(false));
    let observed_bad = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|index| {
            let data = Arc::clone(&data);
            let published = Arc::clone(&published);
            let observed_bad = Arc::clone(&observed_bad);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                if index == 0 {
                    published.store(true, Ordering::Relaxed);
                    thread::yield_now();
                    data.store(42, Ordering::Relaxed);
                } else {
                    while !published.load(Ordering::Relaxed) {
                        thread::yield_now();
                    }
                    if data.load(Ordering::Relaxed) != 42 {
                        observed_bad.store(true, Ordering::SeqCst);
                    }
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }
    observed_bad.load(Ordering::SeqCst)
}

fn producer_consumer(threads: usize) -> bool {
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let done = Arc::new(AtomicBool::new(false));
    let consumed = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|index| {
            let queue = Arc::clone(&queue);
            let done = Arc::clone(&done);
            let consumed = Arc::clone(&consumed);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                if index == 0 {
                    done.store(true, Ordering::SeqCst);
                    thread::yield_now();
                    queue.lock().unwrap().extend(0..QUEUE_ITEMS);
                    return;
                }

                loop {
                    if queue.lock().unwrap().pop_front().is_some() {
                        consumed.fetch_add(1, Ordering::SeqCst);
                    } else if done.load(Ordering::SeqCst) {
                        break;
                    } else {
                        thread::yield_now();
                    }
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }
    consumed.load(Ordering::SeqCst) != QUEUE_ITEMS
}

fn missing_barrier(threads: usize) -> bool {
    let slots = Arc::new(
        (0..threads)
            .map(|_| AtomicBool::new(false))
            .collect::<Vec<_>>(),
    );
    let observed_bad = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|index| {
            let slots = Arc::clone(&slots);
            let observed_bad = Arc::clone(&observed_bad);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                if index == 0 {
                    thread::yield_now();
                    if slots
                        .iter()
                        .skip(1)
                        .any(|slot| !slot.load(Ordering::SeqCst))
                    {
                        observed_bad.store(true, Ordering::SeqCst);
                    }
                } else {
                    thread::yield_now();
                    slots[index].store(true, Ordering::SeqCst);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }
    observed_bad.load(Ordering::SeqCst)
}

fn condvar_lost_wakeup(threads: usize) -> bool {
    let ready = Arc::new(AtomicBool::new(false));
    let lost_wakeup = Arc::new(AtomicBool::new(false));
    let pair = Arc::new((Mutex::new(()), Condvar::new()));
    let start = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();

    for _ in 0..threads - 1 {
        let ready = Arc::clone(&ready);
        let lost_wakeup = Arc::clone(&lost_wakeup);
        let pair = Arc::clone(&pair);
        let start = Arc::clone(&start);
        handles.push(thread::spawn(move || {
            start.wait();
            if ready.load(Ordering::SeqCst) {
                return;
            }

            thread::yield_now();
            let mut guard = pair.0.lock().unwrap();
            if ready.load(Ordering::SeqCst) {
                // A waiter without this recheck would sleep after the only notification.
                lost_wakeup.store(true, Ordering::SeqCst);
                return;
            }
            while !ready.load(Ordering::SeqCst) {
                guard = pair.1.wait(guard).unwrap();
            }
        }));
    }

    let notifier_ready = Arc::clone(&ready);
    let notifier_pair = Arc::clone(&pair);
    let notifier_start = Arc::clone(&start);
    handles.push(thread::spawn(move || {
        notifier_start.wait();
        let _guard = notifier_pair.0.lock().unwrap();
        notifier_ready.store(true, Ordering::SeqCst);
        notifier_pair.1.notify_all();
    }));

    for handle in handles {
        handle.join().unwrap();
    }
    lost_wakeup.load(Ordering::SeqCst)
}

fn mutex_correctness(threads: usize) -> bool {
    let value = Arc::new(Mutex::new(0usize));
    let start = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let value = Arc::clone(&value);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                for round in 0..CONTENTION_ROUNDS {
                    *value.lock().unwrap() += 1;
                    if round % 5 == 0 {
                        thread::yield_now();
                    }
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }
    *value.lock().unwrap() != threads * CONTENTION_ROUNDS
}

fn rwlock_fairness(threads: usize) -> bool {
    let lock = Arc::new(RwLock::new(0usize));
    let writer_acquired = Arc::new(AtomicBool::new(false));
    let reads_before_writer = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();

    for _ in 0..threads - 1 {
        let lock = Arc::clone(&lock);
        let writer_acquired = Arc::clone(&writer_acquired);
        let reads_before_writer = Arc::clone(&reads_before_writer);
        let start = Arc::clone(&start);
        handles.push(thread::spawn(move || {
            start.wait();
            for _ in 0..RWLOCK_ROUNDS {
                let guard = lock.read().unwrap();
                if !writer_acquired.load(Ordering::SeqCst) {
                    reads_before_writer.fetch_add(1, Ordering::SeqCst);
                }
                drop(guard);
                thread::yield_now();
            }
        }));
    }

    let writer_lock = Arc::clone(&lock);
    let writer_acquired_flag = Arc::clone(&writer_acquired);
    let writer_start = Arc::clone(&start);
    handles.push(thread::spawn(move || {
        writer_start.wait();
        *writer_lock.write().unwrap() = 1;
        writer_acquired_flag.store(true, Ordering::SeqCst);
    }));

    for handle in handles {
        handle.join().unwrap();
    }
    reads_before_writer.load(Ordering::SeqCst) == (threads - 1) * RWLOCK_ROUNDS
}

fn store_buffer(threads: usize) -> bool {
    assert!(threads.is_multiple_of(2));
    let start = Arc::new(Barrier::new(threads));
    let results = Arc::new(
        (0..threads)
            .map(|_| AtomicUsize::new(usize::MAX))
            .collect::<Vec<_>>(),
    );
    let mut handles = Vec::new();

    for pair in 0..threads / 2 {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let left_start = Arc::clone(&start);
        let left_results = Arc::clone(&results);
        let left_x = Arc::clone(&x);
        let left_y = Arc::clone(&y);
        handles.push(thread::spawn(move || {
            left_start.wait();
            left_x.store(1, Ordering::Relaxed);
            left_results[pair * 2].store(left_y.load(Ordering::Relaxed), Ordering::Relaxed);
        }));

        let right_start = Arc::clone(&start);
        let right_results = Arc::clone(&results);
        let right_x = Arc::clone(&x);
        let right_y = Arc::clone(&y);
        handles.push(thread::spawn(move || {
            right_start.wait();
            right_y.store(1, Ordering::Relaxed);
            right_results[pair * 2 + 1].store(right_x.load(Ordering::Relaxed), Ordering::Relaxed);
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }
    (0..threads / 2).any(|pair| {
        results[pair * 2].load(Ordering::Relaxed) == 0
            && results[pair * 2 + 1].load(Ordering::Relaxed) == 0
    })
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let category = args.next().expect("stress category is required");
    let threads: usize = args
        .next()
        .expect("thread count is required")
        .parse()
        .expect("thread count must be an integer");
    assert!(threads >= 2);

    let exposed = match category.as_str() {
        "atomic-lost-update" => atomic_lost_update(threads),
        "publish-ordering" => publish_ordering(threads),
        "producer-consumer" => producer_consumer(threads),
        "missing-barrier" => missing_barrier(threads),
        "condvar-lost-wakeup" => condvar_lost_wakeup(threads),
        "mutex-correctness" => mutex_correctness(threads),
        "rwlock-fairness" => rwlock_fairness(threads),
        "store-buffer" => store_buffer(threads),
        _ => panic!("unknown stress category: {category}"),
    };

    println!("category={category} threads={threads} exposed={exposed}");
    if exposed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
