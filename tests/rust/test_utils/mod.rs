// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::io;
use std::io::Write;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub const NANOS_PER_SEC: i64 = 1_000_000_000;

pub fn sleep_race<F>(sleep_function: F) -> u64
where
    F: Fn(),
{
    let flag = Arc::new(AtomicUsize::new(0));
    let flag2 = flag.clone();
    // This work will accomplish an arbitrary number of iterations while the parent thread
    // sleeps:
    let child = std::thread::spawn(move || {
        let mut count = 0;
        let mut spurious = 0.00000001;
        while flag2.load(Ordering::SeqCst) == 0 {
            print!("{} ", count);
            io::stdout().flush().unwrap();
            for _ in 0..50000 {
                count += 1;
                spurious += 0.000001;
                if flag2.load(Ordering::SeqCst) == 1 {
                    break;
                }
            }
        }
        (count, spurious)
    });

    sleep_function();

    let mut tp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        // issue a raw clock_gettime syscall to the OS to get the current time
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut tp);
        tp.tv_nsec += 100_000_000; // 100ms is just a random amount of time that makes the test fast to complete.
        if tp.tv_nsec / NANOS_PER_SEC > 0 {
            tp.tv_sec += tp.tv_nsec / NANOS_PER_SEC;
            tp.tv_nsec %= NANOS_PER_SEC;
        }
        // issue a raw clock_nanosleep syscall to the OS
        libc::clock_nanosleep(
            libc::CLOCK_MONOTONIC,
            libc::TIMER_ABSTIME,
            &tp,
            std::ptr::null_mut(),
        )
    };

    flag.store(1, Ordering::SeqCst);
    let (final_count, spurious) = child.join().unwrap();
    println!("\n Final count: {}", final_count);
    println!("\n Silly number: {}", spurious);
    final_count
}
