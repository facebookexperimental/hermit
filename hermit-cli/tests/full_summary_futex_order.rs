/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `Scheduler::full_summary` must print its futex table in a stable order.
//!
//! `blocked.futex_waiters` is a `HashMap<FutexID, Vec<FutexWaiter>>`
//! (`detcore/src/scheduler.rs`), and a `HashMap` with the default `RandomState`
//! hasher is seeded per process from the OS, so its iteration order changes
//! between runs. The summary is emitted in a WARN record that `--verify-strict`
//! compares, so an unsorted dump is a run-to-run difference in observed output.
//!
//! ⚠️ WHY THIS TEST NEEDS MORE THAN ONE FUTEX KEY, and why it asserts that it
//! got them. A `HashMap` holding ONE key has exactly one iteration order, so a
//! dump taken while a single futex holds waiters is stable whether or not the
//! sort exists -- it certifies nothing. That is not hypothetical: the sort
//! landed with this half of its behaviour undemonstrated precisely because
//! every state then reachable held exactly one futex.
//!
//! Measured with the sort ablated, five runs per stop point: 1 key gave 1
//! distinct dump in 5, 2 keys gave 2, 3 keys gave 5, 7 keys gave 5. Two keys is
//! the minimum that can vary at all but admits only two permutations, so a
//! short sample misses it. This test therefore requires at least three.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const GUEST: &str = "rustbin_multi_futex_block";

/// Range of stop points to search, and the step between them.
///
/// The scheduler's turn numbering is not a stable interface and it moves with
/// the guest's environment -- the same command that parks seven futexes at turn
/// 22 from an interactive shell has not spawned a thread by turn 23 under
/// `cargo test`, because the inherited environment changes how much startup
/// work precedes the spawns. Pinning a number would make this test pass or fail
/// on where it was invoked from, so it searches instead.
const TURN_FIRST: u32 = 12;
const TURN_LAST: u32 = 120;
const TURN_STEP: u32 = 4;

/// Fewer keys than this cannot distinguish a sorted dump from an unsorted one
/// often enough to be worth asserting on.
///
/// Measured against a build with the sort removed: a 7-key dump gave 10
/// distinct orders in 10 runs, while a 3-key dump was stable across the runs
/// this test makes -- 3 keys admit only 6 permutations, so two samples agree by
/// chance about one time in six. An earlier version of this test accepted 3 and
/// PASSED against the unsorted build, i.e. it was inert. Requiring 5 and
/// comparing several runs is what makes a regression actually fail it.
const MIN_FUTEX_KEYS: usize = 5;

/// Repeats compared against the first. Independent processes, so each gets its
/// own `RandomState` seed.
const REPEATS: usize = 4;

fn guest_path() -> PathBuf {
    let binary_directory = Path::new(env!("CARGO_BIN_EXE_hermit"))
        .parent()
        .expect("hermit binary should have a parent directory");
    let guest = binary_directory.join(GUEST);
    if !guest.is_file() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "hermetic_infra_hermit_tests", "--bin", GUEST])
            .status()
            .expect("failed to invoke cargo to build the guest");
        assert!(status.success(), "building {GUEST} failed");
    }
    assert!(guest.is_file(), "{} is missing", guest.display());
    guest
}

/// Run to `turn` and return the futex rows of the summary, in the order printed.
fn futex_rows(guest: &Path, turn: u32) -> Vec<String> {
    let output = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--strict", &format!("--stop-after-turn={turn}")])
        .arg("--")
        .arg(guest)
        .output()
        .expect("failed to start hermit");
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("Private {") || line.starts_with("Shared {"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn full_summary_prints_futex_waiters_in_a_stable_order() {
    let guest = guest_path();

    let (turn, first) = (TURN_FIRST..=TURN_LAST)
        .step_by(TURN_STEP as usize)
        .map(|turn| (turn, futex_rows(&guest, turn)))
        .find(|(_, rows)| rows.len() >= MIN_FUTEX_KEYS)
        .unwrap_or_else(|| {
            panic!(
                "no stop point in {TURN_FIRST}..={TURN_LAST} step {TURN_STEP} parked at least \
                 {MIN_FUTEX_KEYS} distinct futexes, so this test cannot tell a sorted dump from \
                 an unsorted one. Widen the range or fix `{GUEST}` rather than letting it pass \
                 vacuously."
            )
        });

    // Sorted output is a property visible only across runs: one dump is always
    // self-consistent. Each repeat is a fresh process with its own hasher seed.
    for repeat in 0..REPEATS {
        let again = futex_rows(&guest, turn);
        assert_eq!(
            first,
            again,
            "full_summary printed the futex table in a different order on repeat {repeat} at \
             turn {turn} ({} keys). `futex_waiters` is a HashMap, so its iteration order varies \
             between processes; the summary must sort before printing.",
            first.len()
        );
    }
}
