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

const NATIVE_RUNS: usize = 24;
const STRICT_RUNS: usize = 6;
const TIMEOUT_SECONDS: u64 = 30;

static FP_REDUCTION_GUEST: OnceLock<PathBuf> = OnceLock::new();

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

fn fp_reduction_guest() -> &'static Path {
    FP_REDUCTION_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root =
                Path::new(env!("CARGO_TARGET_TMPDIR")).join("fp-reduction-determinism");
            fs::create_dir_all(&build_root).expect("failed to create FP reduction build directory");
            let binary = build_root.join("fp-reduction");

            let mut command = Command::new("cc");
            command
                .args([
                    "-std=c11",
                    "-O2",
                    "-g",
                    "-fopenmp",
                    "-fno-fast-math",
                    "-fno-tree-vectorize",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/fp_reduction_nondeterminism.c"))
                .arg("-o")
                .arg(&binary);
            command_output(command, "FP reduction guest compilation");
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
        output.stdout.starts_with(b"threads=4 bits="),
        "{label} produced unexpected output: {:?}",
        String::from_utf8_lossy(&output.stdout),
    );
    output.stdout
}

fn run_native(iteration: usize) -> Vec<u8> {
    run_with_timeout(
        Command::new(fp_reduction_guest()),
        &format!("native FP reduction iteration {}", iteration + 1),
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
    command.arg(fp_reduction_guest());
    run_with_timeout(
        command,
        &format!("strict FP reduction iteration {}", iteration + 1),
    )
}

#[test]
fn native_parallel_fp_reduction_exposes_low_bit_variation() {
    let outputs: BTreeSet<_> = (0..NATIVE_RUNS).map(run_native).collect();

    assert!(
        outputs.len() > 1,
        "native FP reduction unexpectedly produced one result in {NATIVE_RUNS} runs: {:?}",
        outputs
            .iter()
            .map(|output| String::from_utf8_lossy(output))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn strict_parallel_fp_reduction_is_bit_identical() {
    let expected = run_strict(0);

    for iteration in 1..STRICT_RUNS {
        assert_eq!(
            run_strict(iteration),
            expected,
            "strict FP reduction changed on iteration {}",
            iteration + 1,
        );
    }
}
