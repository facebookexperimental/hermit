/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

// Compile and lint the standalone guest as part of this Cargo integration target.
#[allow(dead_code)]
#[path = "../../tests/stress/scheduling_fairness.rs"]
mod scheduling_fairness_guest;

const RUNS: usize = 5;
const TIMEOUT_SECONDS: u64 = 20;
const COUNTER_TURNS: usize = 64;
const QUEUE_ITEMS: usize = 256;
const QUEUE_CAPACITY: usize = 8;
const WRITER_ROUNDS: usize = 32;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static FAIRNESS_GUEST: OnceLock<PathBuf> = OnceLock::new();

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fairness_guest() -> &'static Path {
    FAIRNESS_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root =
                Path::new(env!("CARGO_TARGET_TMPDIR")).join("thread-scheduling-fairness");
            fs::create_dir_all(&build_root).expect("failed to create fairness build directory");
            let output = build_root.join("scheduling-fairness");
            let mut command = Command::new("rustc");
            command
                .args(["--edition=2024", "-C", "opt-level=2", "-C", "debuginfo=1"])
                .arg(repository.join("tests/stress/scheduling_fairness.rs"))
                .arg("-o")
                .arg(&output);
            let rendered = format!("{command:?}");
            let result = command
                .output()
                .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
            assert!(
                result.status.success(),
                "fairness guest compilation failed: {rendered}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr),
            );
            output
        })
        .as_path()
}

fn run_workload(workload: &str, run: usize) -> String {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=error",
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--",
        ])
        .arg(fairness_guest())
        .arg(workload);
    let rendered = format!("{command:?}");
    let output = command.output().unwrap_or_else(|error| {
        panic!("failed to start {workload} fairness run {run}: {rendered}: {error}")
    });
    assert!(
        output.status.success(),
        "{workload} fairness run {run} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("fairness guest stdout should be UTF-8")
}

fn assert_five_deterministic_runs(workload: &str) -> String {
    let expected = run_workload(workload, 1);
    for run in 2..=RUNS {
        assert_eq!(
            run_workload(workload, run),
            expected,
            "{workload} fairness metrics changed on run {run}"
        );
    }
    expected
}

fn metric<'a>(output: &'a str, name: &str) -> &'a str {
    output
        .split_whitespace()
        .find_map(|field| field.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("missing {name} metric in {output:?}"))
}

#[test]
fn four_runnable_threads_receive_round_robin_progress() {
    let _guard = hermit_run_lock();
    let output = assert_five_deterministic_runs("counter");
    let counts = metric(&output, "counts")
        .split(',')
        .map(|value| value.parse::<usize>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(counts, vec![COUNTER_TURNS; 4]);

    let max_gaps = metric(&output, "max_gaps")
        .split(',')
        .map(|value| value.parse::<usize>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(max_gaps.len(), 4);
    assert!(
        max_gaps.iter().all(|gap| *gap <= 3),
        "a runnable worker waited behind more than the other three workers: {output}"
    );
    assert!(metric(&output, "worst").parse::<usize>().unwrap() <= 3);
}

#[test]
fn bounded_buffer_producer_and_consumers_complete() {
    let _guard = hermit_run_lock();
    let output = assert_five_deterministic_runs("producer-consumer");
    assert_eq!(metric(&output, "produced"), QUEUE_ITEMS.to_string());
    assert_eq!(metric(&output, "consumed"), QUEUE_ITEMS.to_string());
    assert_eq!(metric(&output, "capacity"), QUEUE_CAPACITY.to_string());
    assert!(
        metric(&output, "max_consumer_streak")
            .parse::<usize>()
            .unwrap()
            <= QUEUE_CAPACITY,
        "producer did not resume before the bounded queue drained: {output}"
    );
}

#[test]
fn rwlock_writer_is_not_starved_by_readers() {
    let _guard = hermit_run_lock();
    let output = assert_five_deterministic_runs("rwlock");
    assert_eq!(metric(&output, "writes"), WRITER_ROUNDS.to_string());
    assert!(metric(&output, "reads").parse::<usize>().unwrap() > 0);
    assert!(
        metric(&output, "max_reads_while_writer_waiting")
            .parse::<usize>()
            .unwrap()
            <= 3,
        "writer waited behind more than one acquisition per reader: {output}"
    );
}
