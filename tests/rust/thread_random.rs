// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! Sequentially spawned threads have different RNG states.
//!
//! This is important, especially for modes that use the thread local RNG to
//! make scheduling decisions. Otherwise, two threads with RNG- and branch-
//! identical workloads (e.g. busywaiting, matrix multiplication, sleeping)
//! would be scheduled identically.

use std::sync::Arc;
use std::sync::Mutex;

const NUMBERS: usize = 20;
const THREADS: usize = 10;
type RngData = Arc<Mutex<[u64; NUMBERS]>>;

fn getrandom(store: RngData) {
    let mut m = store.lock().unwrap();
    for i in 0..m.len() {
        let mut x = 0u64;
        let res =
            unsafe { libc::getrandom(&mut x as *mut _ as *mut _, std::mem::size_of_val(&x), 0) };
        assert_eq!(res, std::mem::size_of_val(&x) as _);
        m[i] = x;
    }
}

fn main() {
    let mut vals: Vec<RngData> = Vec::new();
    for _ in 0..THREADS {
        vals.push(Arc::new(Mutex::new(Default::default())));
    }
    let mut handles = Vec::new();
    for arc in vals.iter() {
        let a = Arc::clone(arc);
        handles.push(std::thread::spawn(move || getrandom(a)));
    }
    handles.into_iter().for_each(|h| h.join().unwrap());
    let vals = vals
        .into_iter()
        .map(|a| Vec::from(Arc::try_unwrap(a).unwrap().into_inner().unwrap()))
        .collect::<Vec<Vec<u64>>>();
    assert_eq!(vals.len(), THREADS);
    println!("{:x?}", vals); // verify values on stdout across runs

    // N.B.: if we could guarantee a cryptographic-quality RNG, it should
    // be fine to assert ALL values unequal, as even with the birthday paradox,
    // it would take ~2^32 numbers before a collision of 64 bit random values.
    //
    // However, since we can't count on the RNG to that extent, we simply ensure that each
    // thread has a difference somewhere in their sequence of NUMBERS values, i.e. that
    // the threads don't have IDENTICAL prng streams because of a specific hermit bug.
    for i in 0..vals.len() {
        assert_eq!(vals[i].len(), NUMBERS);
        for j in 0..i {
            assert_ne!(vals[i], vals[j]);
        }
    }
    println!("Test completed successfully.");
}
