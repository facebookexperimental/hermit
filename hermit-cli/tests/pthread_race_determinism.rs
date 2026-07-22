/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

const EXPECTED_COUNTER: u64 = 2_000_000;
const NATIVE_RUNS: usize = 24;
const STRICT_RUNS: usize = 6;
const THREAD_COUNT: usize = 8;
const TIMEOUT_SECONDS: u64 = 30;

static PTHREAD_RACE_GUEST: OnceLock<PathBuf> = OnceLock::new();

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn pthread_race_guest() -> &'static Path {
    PTHREAD_RACE_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root =
                Path::new(env!("CARGO_TARGET_TMPDIR")).join("pthread-race-determinism");
            fs::create_dir_all(&build_root).expect("failed to create pthread race build directory");
            let binary = build_root.join("pthread-race");

            let mut command = Command::new("cc");
            command
                .args([
                    "-std=c11",
                    "-O2",
                    "-g",
                    "-pthread",
                    "-D_GNU_SOURCE",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/pthread_race_nondeterminism.c"))
                .arg("-o")
                .arg(&binary);
            command_output(command, "pthread race guest compilation");
            binary
        })
        .as_path()
}

fn run_with_timeout(command: Command, label: &str) -> Vec<u8> {
    let mut timeout = Command::new("timeout");
    timeout
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(command.get_program())
        .args(command.get_args());
    let output = command_output(timeout, label);
    assert!(
        output.stdout.starts_with(b"counter="),
        "{label} produced unexpected output: {:?}",
        String::from_utf8_lossy(&output.stdout),
    );
    output.stdout
}

fn run_native(iteration: usize) -> Vec<u8> {
    run_with_timeout(
        Command::new(pthread_race_guest()),
        &format!("native pthread race iteration {}", iteration + 1),
    )
}

fn run_strict(iteration: usize) -> Vec<u8> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "run",
        "--strict",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--preemption-timeout=disabled",
        "--",
    ]);
    command.arg(pthread_race_guest());
    run_with_timeout(
        command,
        &format!("strict pthread race iteration {}", iteration + 1),
    )
}

fn parse_result(output: &[u8]) -> (u64, String) {
    let output = std::str::from_utf8(output)
        .expect("pthread race output should be UTF-8")
        .trim();
    let mut fields = output.split_ascii_whitespace();

    let counter = fields
        .next()
        .and_then(|field| field.strip_prefix("counter="))
        .expect("pthread race output should contain a counter")
        .parse()
        .expect("pthread race counter should be an integer");
    let expected = fields
        .next()
        .and_then(|field| field.strip_prefix("expected="))
        .expect("pthread race output should contain the expected counter")
        .parse::<u64>()
        .expect("expected pthread race counter should be an integer");
    assert_eq!(expected, EXPECTED_COUNTER);

    let order = fields
        .next()
        .and_then(|field| field.strip_prefix("order="))
        .expect("pthread race output should contain completion order");
    assert_eq!(
        order.split(',').count(),
        THREAD_COUNT,
        "pthread race output should list every thread: {output}"
    );
    assert!(
        fields.next().is_none(),
        "pthread race output contained extra fields: {output}"
    );

    (counter, order.to_owned())
}

#[test]
fn native_pthread_race_exposes_counter_and_order_variation() {
    let outputs: Vec<_> = (0..NATIVE_RUNS).map(run_native).collect();
    let results: Vec<_> = outputs.iter().map(|output| parse_result(output)).collect();
    let counters: BTreeSet<_> = results.iter().map(|(counter, _)| *counter).collect();
    let orders: BTreeSet<_> = results.iter().map(|(_, order)| order).collect();

    assert!(
        counters.len() > 1,
        "native pthread race produced one counter in {NATIVE_RUNS} runs: {counters:?}"
    );
    assert!(
        orders.len() > 1,
        "native pthread race produced one completion order in {NATIVE_RUNS} runs: {orders:?}"
    );
}

#[test]
fn strict_pthread_race_is_deterministic() {
    let expected = run_strict(0);
    let (counter, _) = parse_result(&expected);
    assert_eq!(
        counter, EXPECTED_COUNTER,
        "strict execution should serialize all counter updates"
    );

    for iteration in 1..STRICT_RUNS {
        assert_eq!(
            run_strict(iteration),
            expected,
            "strict pthread race changed on iteration {}",
            iteration + 1,
        );
    }
}
