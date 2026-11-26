/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Explicitly checks that schedules are explored over four joined
//! CAS handoff phases in one execution.
//!
//! Each phase is the same three-CAS handoff: the early thread CASes
//! 1 -> 2, spins, then CASes 3 -> 4; the late thread spins, then
//! CASes 2 -> 3. A phase reaches the antagonistic final value 4
//! only when the late thread's CAS lands inside the early thread's
//! middle spin. One execution fails iff any phase reaches 4, so both
//! passing and failing executions remain possible across seeds.
//!
//! The last phase uses the original test's 60,000 / 20,000 spin
//! amounts and is reported separately. It is an original-amounts
//! phase, not the original execution: three phases precede it and it
//! spawns early-then-late.

use std::ptr::read_volatile;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;

const FINAL_VALUE: usize = 4;
/// PLANTED NO-RACE CONTROL: this value is one the atomic never
/// holds (it only ever takes 1, 2, or a final 4 the early thread
/// cannot reach without the middle step), so the middle CAS can
/// never succeed and no phase can reach the antagonistic final
/// value. Every spin, spawn, join and print is otherwise in exactly
/// the order of cas_sequence.rs.
const MIDDLE_FROM: usize = 99;

#[derive(Clone, Copy)]
struct Phase {
    late_spin: usize,
    early_mid_spin: usize,
    late_first: bool,
}

/// Deterministic phases retained from the measured 24-round sweep.
/// In that sweep's numbering these were rounds 8, 11, 16 and 23.
/// Phase identity includes spawn order, so it is part of the table
/// rather than a consequence of a phase's position here.
const PHASES: [Phase; 4] = [
    Phase {
        late_spin: 0,
        early_mid_spin: 10_000,
        late_first: true,
    },
    Phase {
        late_spin: 15_000,
        early_mid_spin: 10_000,
        late_first: false,
    },
    Phase {
        late_spin: 0,
        early_mid_spin: 20_000,
        late_first: true,
    },
    Phase {
        late_spin: 60_000,
        early_mid_spin: 20_000,
        late_first: false,
    },
];
/// Index of the phase using the original 60,000 / 20,000 amounts.
const ORIGINAL_AMOUNTS_ROUND: usize = 3;

#[inline(never)]
fn do_work(iters: usize) {
    let mut x = iters;
    while unsafe { read_volatile(&x as *const _) } > 0usize {
        x -= 1;
    }
}

#[inline(never)]
fn take_from(a: &AtomicUsize, from: usize, to: usize) {
    a.compare_exchange(from, to, SeqCst, SeqCst).ok();
}

#[inline(never)]
fn late_thread(var: Arc<AtomicUsize>, spin: usize) {
    do_work(spin);
    take_from(&var, MIDDLE_FROM, 3);
}

#[inline(never)]
fn early_thread(var: Arc<AtomicUsize>, mid_spin: usize) {
    take_from(&var, 1, 2);
    do_work(mid_spin); // <- the middle CAS must land in this window
    take_from(&var, 3, FINAL_VALUE);
}

fn one_round(phase: Phase) -> usize {
    let value = Arc::new(AtomicUsize::new(1));
    let late_value = Arc::clone(&value);
    let early_value = Arc::clone(&value);
    let (late, early) = if phase.late_first {
        let late = std::thread::spawn(move || late_thread(late_value, phase.late_spin));
        let early = std::thread::spawn(move || early_thread(early_value, phase.early_mid_spin));
        (late, early)
    } else {
        let early = std::thread::spawn(move || early_thread(early_value, phase.early_mid_spin));
        let late = std::thread::spawn(move || late_thread(late_value, phase.late_spin));
        (late, early)
    };
    late.join().unwrap();
    early.join().unwrap();
    Arc::try_unwrap(value).unwrap().into_inner()
}

fn main() {
    let mut hits = Vec::new();
    let mut final_counts = [0usize; FINAL_VALUE + 1];
    let mut original_amounts_final = 0usize;
    for (round, phase) in PHASES.into_iter().enumerate() {
        let final_value = one_round(phase);
        if final_value <= FINAL_VALUE {
            final_counts[final_value] += 1;
        }
        if round == ORIGINAL_AMOUNTS_ROUND {
            original_amounts_final = final_value;
        }
        if final_value == FINAL_VALUE {
            hits.push(round);
        }
    }

    println!(
        "Control handoff rounds: {}; finals {{2: {}, 3: {}, 4: {}}}; original-amounts round {ORIGINAL_AMOUNTS_ROUND} final: {original_amounts_final}; antagonistic rounds: {hits:?}",
        PHASES.len(),
        final_counts[2],
        final_counts[3],
        final_counts[4]
    );
    if !hits.is_empty() {
        println!("Antagonistic schedule reached, failing.");
        std::process::exit(1);
    }
    println!("Did not find antagonistic schedule. Succeeding.");
}
