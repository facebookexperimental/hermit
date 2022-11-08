// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! Race a (parent) thread sleeping, with one spinning/printing.

use std::io;
use std::io::Write;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn run_test() -> u64 {
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

    let tp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    unsafe {
        // issue a raw nanosleep syscall to the OS
        libc::nanosleep(&tp, std::ptr::null_mut())
    };

    flag.store(1, Ordering::SeqCst);
    let (final_count, spurious) = child.join().unwrap();
    println!("\n Final count: {}", final_count);
    println!("\n Silly number: {}", spurious);
    final_count
}

/// Raw execution exposes nondeterminism with this race.
fn main() {
    let final_count = run_test();
    assert!(final_count > 0); // Safe bet with a 100ms sleep.
}
