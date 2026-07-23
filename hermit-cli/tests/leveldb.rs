/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

const LEVELDB_BUILD_DIR: &str = "HERMIT_LEVELDB_BUILD_DIR";

// These cases exercise database creation, reads/writes, snapshots, recovery,
// compaction, locks, filters, manifests, logs, write batches, and in-memory I/O
// without LevelDB's long time-based concurrent stress loops.
const FOCUSED_FILTER: &str = concat!(
    "DBTest.ReadWrite:",
    "DBTest.PutDeleteGet:",
    "DBTest.GetSnapshot:",
    "DBTest.Recover:",
    "DBTest.ManualCompaction:",
    "DBTest.Locking:",
    "DBTest.BloomFilter:",
    "DBTest.DestroyOpenDB:",
    "DBTest.FilesDeletedAfterCompaction:",
    "RecoveryTest.ManifestReused:",
    "RecoveryTest.MultipleLogFiles:",
    "LogTest.ReadWrite:",
    "WriteBatchTest.Multiple:",
    "MemEnvTest.ReadWrite:",
    "TableTest.ApproximateOffsetOfPlain",
);

fn configured_build_dir() -> Option<PathBuf> {
    let Some(path) = env::var_os(LEVELDB_BUILD_DIR) else {
        eprintln!(
            "skipping LevelDB integration test: {LEVELDB_BUILD_DIR} is not set; \
             run hermit-cli/tests/prepare_leveldb.sh first"
        );
        return None;
    };
    Some(PathBuf::from(path))
}

fn required_build_dir() -> PathBuf {
    configured_build_dir().unwrap_or_else(|| {
        panic!("ignored LevelDB test was explicitly requested without setting {LEVELDB_BUILD_DIR}")
    })
}

fn executable(build_dir: &Path, name: &str) -> PathBuf {
    let path = build_dir.join(name);
    let metadata = fs::metadata(&path)
        .unwrap_or_else(|error| panic!("cannot inspect {}: {error}", path.display()));
    assert!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "{} is not an executable file",
        path.display()
    );
    path
}

fn strict_run(binary: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=error", "run", "--strict", "--base-env=minimal", "--"])
        .arg(binary)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {} under Hermit: {error}", binary.display()))
}

fn assert_success(output: &Output, label: &str, run: usize) {
    assert!(
        output.status.success(),
        "{label} run {run} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_deterministic(binary: &Path, args: &[&str], label: &str) {
    let first = strict_run(binary, args);
    assert_success(&first, label, 1);
    let second = strict_run(binary, args);
    assert_success(&second, label, 2);

    assert_eq!(first.status, second.status, "{label} exit status differed");
    assert!(
        first.stdout == second.stdout,
        "{label} stdout differed\nrun 1:\n{}\nrun 2:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&second.stdout),
    );
    assert!(
        first.stderr == second.stderr,
        "{label} stderr differed\nrun 1:\n{}\nrun 2:\n{}",
        String::from_utf8_lossy(&first.stderr),
        String::from_utf8_lossy(&second.stderr),
    );
}

#[test]
fn focused_leveldb_tests_are_deterministic_under_strict() {
    let Some(build_dir) = configured_build_dir() else {
        return;
    };

    assert_deterministic(&executable(&build_dir, "c_test"), &[], "LevelDB c_test");

    let filter = format!("--gtest_filter={FOCUSED_FILTER}");
    assert_deterministic(
        &executable(&build_dir, "leveldb_tests"),
        &[&filter],
        "focused LevelDB suite",
    );
}

#[test]
#[ignore = "the full LevelDB suite contains long concurrent and randomized stress cases"]
fn full_leveldb_suite_is_deterministic_under_strict() {
    let build_dir = required_build_dir();
    assert_deterministic(
        &executable(&build_dir, "leveldb_tests"),
        &[],
        "full LevelDB suite",
    );
}

#[test]
#[ignore = "LevelDB's repeated fork+exec close-on-exec checks are slow under ptrace"]
fn leveldb_env_posix_is_deterministic_under_strict() {
    let build_dir = required_build_dir();
    assert_deterministic(
        &executable(&build_dir, "env_posix_test"),
        &[],
        "LevelDB env_posix_test",
    );
}
