//! Regression for hermit_manifest_plan::exec_safe_fs under a spawn storm
//! (dh-lnslwh): while other threads spawn continuously, every executable the
//! helpers create runs at once. Each one is run immediately, which is where an
//! in-process write was refused with ETXTBSY.
//!
//! This is its own test binary because the storm must not share a process
//! with the library's tests. Inside the parallel libtest process it failed
//! the runner's CPU-budget tests in 7 of 20 full `cargo test --lib` runs, and
//! still in 5 of 8 when the storm ran in a child of that process; with the
//! case left out, 0 of 10 failed. Cargo runs test binaries one after another,
//! and Nextest runs each test in a process of its own, so neither runs this
//! storm beside those tests' in-process parallel load.
//!
//! The storm costs CPU on purpose, and Nextest charges a test for its
//! children against the default 22 CPU-s cap. Six spawners and 100 iterations
//! measured 7.2 to 7.6 CPU-s in about 1.2 s of wall time. 8 and 200 measured
//! 17 to 18 CPU-s. Against an in-process write or copy, 6 and 100 still found
//! 6 to 23 busy executables of 300 in each of 13 runs per mutant; one fails
//! the test it falls in.
//!
//! Nearly all of that CPU is system time spent spawning, and system time grows
//! with kernel contention from elsewhere on the host, which user time does
//! not. In 67 full validations on one development host from 2026-11-20 to
//! 2026-11-25, 65 finished the 100 iterations in 6.4 to 17.7 CPU-s (user 1.9
//! to 3.9, system 4.4 to 13.7). The other 2 were stopped at the cap with only
//! part of their user work done: one on 2026-11-20 (system 21.3 CPU-s), and
//! one on 2026-11-25, where every attempt was stopped about halfway (system
//! 20.8 to 21.0). In that second validation, the 2,328 tests of the node spent
//! the same user time as a validation that morning (x1.01) and 2.36 times the
//! system time. So the 100 iterations are split across `PARTS` tests of
//! `ITERATIONS_PER_PART`, each under its own cap. Every step still runs under
//! the same grant, so on any attempt a mutant that leaves a step busy fails
//! the part that holds that step. Nextest retries a failed test twice
//! (`retries = 2`), and a retry reruns only that part's 75 steps. So at the
//! weakest measured busy rate, a broken helper is more likely than before to
//! end FLAKY rather than FAIL. FLAKY is recorded, never silent.
//!
//! The storm is bounded by work, not by wall time. Before each of the 300
//! write-and-run steps the main thread grants SPAWNS_PER_STEP spawns, and the
//! spawners park, without spinning, while none are left. Unbounded, the
//! spawners ran until the main loop ended, so on a loaded host, where the main
//! loop is slow, the test ran out of its CPU budget (one merge-queue run was
//! stopped at 5.3 s) without finding a busy executable. They park without a
//! timeout: a timed park woke each one every millisecond for as long as the
//! main loop ran, a cost that grows with wall time. A grant and the final stop
//! unpark them.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use hermit_manifest_plan::exec_safe_fs::copy;
use hermit_manifest_plan::exec_safe_fs::write_executable;

const SCRIPT: &str = "#!/bin/sh\nexit 0\n";

/// The spawns granted to the six spawners together before each write-and-run
/// step. The unbounded storm completed a median of 29 spawns per step (p10 24,
/// p90 33; 8,496 to 8,628 per run) in each of 5 passing runs, counted between
/// the starts of consecutive steps. The grant matches that measured median, so
/// a run starts at most 29 spawns per step on average (8,700 in all). A step
/// draws more than 29 only from grants that earlier steps left unused, and a
/// run whose steps would have drawn more than the median now gets fewer.
const SPAWNS_PER_STEP: u64 = 29;

/// The tests the storm's 100 iterations are split across.
const PARTS: usize = 4;

/// The iterations, of three write-and-run steps each, that one part runs.
const ITERATIONS_PER_PART: usize = 100 / PARTS;

// PARTS divides the 100 iterations evenly. This does not check the wrappers
// below, which must call storm(0) to storm(PARTS - 1), each once.
const _: () = assert!(ITERATIONS_PER_PART * PARTS == 100);

/// A fresh directory under the temporary root, removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Stops the spawners and unparks each when dropped. `storm` builds it once
/// all six spawners have started, so from then on every way out of `storm`,
/// a panic included, stops them and none parks forever. (If starting a
/// spawner panics, the ones already started stay parked until the process
/// exits; that path has no join, so nothing hangs.) A spawner leaves
/// its loop only when it reads the stop. One that read no stop and no grant
/// and then parks takes the unpark that follows the store, even an unpark
/// that came before it parked, and reads the stop on its next turn.
struct Stopper {
    stop: Arc<AtomicBool>,
    spawners: Vec<std::thread::Thread>,
}

impl Drop for Stopper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for spawner in &self.spawners {
            spawner.unpark();
        }
    }
}

#[test]
fn helpers_create_executables_that_are_never_busy_under_concurrent_spawns_part_1_of_4() {
    storm(0);
}

#[test]
fn helpers_create_executables_that_are_never_busy_under_concurrent_spawns_part_2_of_4() {
    storm(1);
}

#[test]
fn helpers_create_executables_that_are_never_busy_under_concurrent_spawns_part_3_of_4() {
    storm(2);
}

#[test]
fn helpers_create_executables_that_are_never_busy_under_concurrent_spawns_part_4_of_4() {
    storm(3);
}

/// Runs part `part` (from 0) of the storm's iterations beside its own six
/// spawners, and fails if any executable the helpers created was busy.
fn storm(part: usize) {
    assert!(part < PARTS);
    let iterations = part * ITERATIONS_PER_PART..(part + 1) * ITERATIONS_PER_PART;
    // Keyed by part as well: `cargo test` runs the parts in one process.
    let dir = Scratch(std::env::temp_dir().join(format!(
        "exec-safe-fs-spawn-storm-{}-{part}",
        std::process::id()
    )));
    std::fs::create_dir(&dir.0).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    // Spawns granted and not yet started; the storm's only CPU budget.
    let allowance = Arc::new(AtomicU64::new(0));
    let spawners = (0..6)
        .map(|_| {
            let stop = stop.clone();
            let allowance = allowance.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // Take one granted spawn, if there is one.
                    let mut left = allowance.load(Ordering::Acquire);
                    let granted = loop {
                        if left == 0 {
                            break false;
                        }
                        match allowance.compare_exchange_weak(
                            left,
                            left - 1,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => break true,
                            Err(now) => left = now,
                        }
                    };
                    if granted {
                        let _ = Command::new("true").status();
                    } else {
                        // A grant, or the stop, unparks it.
                        std::thread::park();
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    let stopper = Stopper {
        stop: stop.clone(),
        spawners: spawners
            .iter()
            .map(|spawner| spawner.thread().clone())
            .collect(),
    };
    let grant = || {
        allowance.fetch_add(SPAWNS_PER_STEP, Ordering::AcqRel);
        for spawner in &spawners {
            spawner.thread().unpark();
        }
    };
    let source = dir.0.join("source");
    write_executable(&source, SCRIPT, 0o755).unwrap();
    let mut failures = Vec::new();
    let mut run = |path: &Path, want: i32| match Command::new(path).status() {
        Ok(status) if status.code() == Some(want) => {}
        other => failures.push(format!("{}: {other:?}", path.display())),
    };
    for index in iterations {
        let written = dir.0.join(format!("written-{index}"));
        grant();
        write_executable(&written, SCRIPT, 0o700).unwrap();
        run(&written, 0);
        // Replacing in place must not reuse the executed inode either.
        grant();
        write_executable(&written, "#!/bin/sh\nexit 3\n", 0o700).unwrap();
        run(&written, 3);
        let copied = dir.0.join(format!("copied-{index}"));
        grant();
        copy(&source, &copied).unwrap();
        run(&copied, 0);
    }
    drop(stopper);
    for spawner in spawners {
        spawner.join().unwrap();
    }
    assert!(failures.is_empty(), "{} busy: {failures:?}", failures.len());
}
