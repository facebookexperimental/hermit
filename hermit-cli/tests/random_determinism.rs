/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::process::Command;

fn compile_guest(output: &Path) {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).expect("failed to create random guest build directory");
    }
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let result = Command::new("cc")
        .args(["-O2", "-g", "-pthread", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/random_sources.c"))
        .arg("-o")
        .arg(output)
        .output()
        .expect("failed to start random guest compilation");
    assert!(
        result.status.success(),
        "random guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn run_guest(guest: &Path, seed: u64) -> Vec<u8> {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
        ])
        .arg(format!("--rng-seed={seed}"))
        .arg(guest)
        .output()
        .expect("failed to run random guest under Hermit");
    assert!(
        output.status.success(),
        "random guest failed for seed {seed}:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn random_sources_repeat_across_runs_and_change_with_seed() {
    let guest = Path::new(env!("CARGO_TARGET_TMPDIR")).join("random-determinism/random-sources");
    compile_guest(&guest);

    let expected = run_guest(&guest, 17);
    assert!(!expected.is_empty());
    for _ in 1..5 {
        assert_eq!(run_guest(&guest, 17), expected);
    }
    assert_ne!(run_guest(&guest, 18), expected);
}
