/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A schedule replay of a --chaos recording follows the recorded slice ends
// rather than drawing chaos timeslices of its own: two threads spin through
// branch runs longer than a typical chaos slice and print the virtual clock
// after each, so a replay that preempts at a point of its own leaves the
// recording (--die-on-desync) or prints other times.
//
// Both runs are --strict: a replay follows its record exactly only where the
// retired-branch counter passes Reverie's validation, and where it does not
// (an AMD Zen host without the SpecLockMap fix) Hermit refuses the run, as for
// the other strict fixtures, rather than let skid make this fixture pass or
// fail by chance. The record and the outputs go in the fresh working
// directory the lit runner makes for the fixture.
//
// RUN: %hermit run --strict --chaos --seed=3 --record-preemptions-to=r.json -- %me > rec && %hermit run --strict --chaos --seed=3 --die-on-desync --replay-schedule-from=r.json -- %me > rep && diff rec rep

use std::hint::black_box;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

fn work(id: u64) {
    for round in 0..6 {
        let mut sink = 0u64;
        for i in 0..3_000_000u64 {
            sink = sink.wrapping_add(black_box(i ^ id));
        }
        black_box(sink);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        println!("t{id} r{round} {}.{:09}", now.as_secs(), now.subsec_nanos());
    }
}

fn main() {
    let other = std::thread::spawn(|| work(1));
    work(2);
    other.join().unwrap();
}
