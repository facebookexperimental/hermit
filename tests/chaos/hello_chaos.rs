// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// This simply tests that two schedules are possible with two threads and no dummy work.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;

fn main() {
    let d = Arc::new(AtomicUsize::new(0));
    let d1 = Arc::clone(&d);
    let h1 = std::thread::spawn(move || {
        d1.store(1, SeqCst);
    });
    d.store(2, SeqCst);
    h1.join().unwrap();

    let val = Arc::try_unwrap(d).unwrap().into_inner();
    println!("Final value: {}", val);
    if val == 1 {
        println!("Parent went first, arbitrarily counting that as an error.");
        std::process::exit(1);
    } else {
        println!("Child went first, counting that as success");
    }
}
