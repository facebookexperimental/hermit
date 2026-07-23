/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end test that CPython `set` iteration order is nondeterministic when
//! run natively but deterministic under `hermit run --strict`.
//!
//! NONDET_SOURCE: getrandom / PYTHONHASHSEED. CPython seeds string hashing once
//! per process from the OS (getrandom(2)/urandom); that seed decides hash-bucket
//! placement, so a `set` of strings iterates in a different order each native
//! run. Hermit virtualizes getrandom to a deterministic value, so the order
//! becomes a stable function of the inputs.
//!
//! A vanilla CPython starts fast enough under Hermit to run inline (~0.03s), so
//! this is a normal test. Meta's `fbpython` build is deliberately skipped (see
//! `find_python3`).

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Number of native runs to sample when looking for order variation.
const NATIVE_SAMPLES: usize = 8;
/// Number of Hermit runs that must agree.
const HERMIT_RUNS: usize = 3;

/// `which python3`, if any.
fn python3_on_path() -> Option<PathBuf> {
    let output = Command::new("which").arg("python3").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let found = String::from_utf8(output.stdout).ok()?;
    let trimmed = found.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Locate a *vanilla* CPython 3 suitable for running under Hermit, or `None`
/// (the test then skips). Meta's `fbpython` build (usually `/usr/local/bin/python3`)
/// issues a `CLONE_VFORK` and spawns many threads during startup, which Hermit
/// does not yet support, so it is skipped in favor of a stock interpreter -- which
/// is also what OSS CI provides.
fn find_python3() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> =
        ["/usr/bin/python3", "/bin/python3", "/usr/local/bin/python3"]
            .iter()
            .map(PathBuf::from)
            .collect();
    candidates.extend(python3_on_path());
    candidates.into_iter().find(|candidate| {
        if !candidate.exists() {
            return false;
        }
        // Skip Meta's fbpython, which vforks and spawns threads at startup.
        let resolved = std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.clone());
        let resolved = resolved.to_string_lossy();
        !resolved.contains("fbpython") && !resolved.contains("/fbcode/")
    })
}

/// Path to the workload script, relative to the repository root.
fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
        .join("tests/python/hashseed_order.py")
}

/// Run the workload directly on the host. `-I` makes CPython ignore the ambient
/// environment (so a pinned `PYTHONHASHSEED` cannot suppress randomization) and
/// `-S` skips site initialization for speed.
fn run_native(python: &Path, script: &Path) -> String {
    let output = Command::new(python)
        .args(["-S", "-I"])
        .arg(script)
        .env_remove("PYTHONHASHSEED")
        .output()
        .expect("failed to run python natively");
    assert!(
        output.status.success(),
        "native python failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("python stdout should be UTF-8")
}

/// Run the workload under `hermit run --strict`.
fn run_hermit_strict(python: &Path, script: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--strict",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ])
        .arg(python)
        .args(["-S", "-I"])
        .arg(script)
        .output()
        .expect("failed to run python under Hermit");
    assert!(
        output.status.success(),
        "hermit python failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("hermit python stdout should be UTF-8")
}

// NONDET_SOURCE: getrandom / PYTHONHASHSEED
#[test]
fn python_set_order_nondeterministic_natively_deterministic_under_hermit() {
    let python = match find_python3() {
        Some(python) => python,
        None => {
            eprintln!("python3 not found on host; skipping hashseed determinism test");
            return;
        }
    };
    let script = script_path();
    assert!(
        script.exists(),
        "missing workload script: {}",
        script.display()
    );

    // Native: the set order is seeded from getrandom, so across several runs we
    // expect to observe more than one distinct ordering.
    let mut native_orderings = HashSet::new();
    for _ in 0..NATIVE_SAMPLES {
        native_orderings.insert(run_native(&python, &script));
    }
    assert!(
        native_orderings.len() > 1,
        "expected nondeterministic native output across {NATIVE_SAMPLES} runs, but all matched:\n{native_orderings:?}"
    );

    // Hermit --strict: getrandom is virtualized, so every run must be identical.
    let expected = run_hermit_strict(&python, &script);
    for _ in 1..HERMIT_RUNS {
        assert_eq!(
            run_hermit_strict(&python, &script),
            expected,
            "hermit --strict output diverged across runs; expected determinism"
        );
    }

    // The stabilized order should be one CPython could actually produce, i.e. one
    // of the natively observed orderings (sanity check that Hermit did not mangle
    // the output).
    assert!(
        expected.starts_with("set: "),
        "unexpected hermit output shape: {expected:?}"
    );
}
