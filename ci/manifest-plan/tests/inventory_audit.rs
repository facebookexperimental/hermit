/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Prove the public inventory audit can both accept and refuse.
//!
//! The negative case invokes `test-harness audit-inventory`, not an extracted
//! helper. A private Git index adds one qualifying path without touching the
//! shared checkout, so concurrent test processes never observe the mutation.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

const UNREGISTERED: &str = "tests/inventory-audit-unregistered-control.c";
const EMPTY_BLOB: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("ci/manifest-plan must sit two levels below the repository root")
        .to_path_buf()
}

fn run_audit(root: &Path, index: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_test-harness"))
        .arg("audit-inventory")
        .current_dir(root)
        .env("GIT_INDEX_FILE", index)
        .output()
        .expect("run test-harness audit-inventory")
}

#[test]
fn audit_inventory_refuses_an_unregistered_test_file() {
    let root = repo_root();
    let git_index = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-path", "index"])
        .current_dir(&root)
        .output()
        .expect("locate the repository index");
    assert!(git_index.status.success(), "cannot locate Git index");
    let git_index = PathBuf::from(
        String::from_utf8(git_index.stdout)
            .expect("Git index path must be UTF-8")
            .trim(),
    );
    let scratch = std::env::temp_dir().join(format!(
        "hermit-inventory-audit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&scratch).expect("create inventory audit scratch directory");
    let private_index = scratch.join("index");
    std::fs::copy(&git_index, &private_index).expect("copy Git index");

    let control = run_audit(&root, &private_index);
    assert_eq!(
        control.status.code(),
        Some(0),
        "registered repository control must pass; stderr: {}",
        String::from_utf8_lossy(&control.stderr)
    );

    let update = Command::new("git")
        .args([
            "update-index",
            "--add",
            "--info-only",
            "--cacheinfo",
            "100644",
            EMPTY_BLOB,
            UNREGISTERED,
        ])
        .current_dir(&root)
        .env("GIT_INDEX_FILE", &private_index)
        .output()
        .expect("add an unregistered test path to the private Git index");
    assert!(
        update.status.success(),
        "cannot plant unregistered test path: {}",
        String::from_utf8_lossy(&update.stderr)
    );

    let refusal = run_audit(&root, &private_index);
    let stderr = String::from_utf8_lossy(&refusal.stderr);
    assert_eq!(
        refusal.status.code(),
        Some(2),
        "the actual audit front door accepted an unregistered test path; stderr: {stderr}"
    );
    assert!(
        stderr.contains(UNREGISTERED) && stderr.contains("unregistered="),
        "the refusal must identify the missing registration, not fail for an unrelated reason: {stderr}"
    );

    std::fs::remove_dir_all(&scratch).expect("remove inventory audit scratch directory");
}
