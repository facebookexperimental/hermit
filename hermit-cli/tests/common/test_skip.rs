/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The runtime skip marker, shared by test crates through `#[path]`. Its
//! behaviour is that of `tests/cli.rs`'s `record_test_skip` and
//! `record_test_skip_to`, which keep their own copy.

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

/// Record a runtime skip for `test`: the test started, found it could not
/// run, and returns normally. The marker line goes to stdout for humans and,
/// when the runner set `HERMIT_TEST_SKIP_FILE`, as one `write()` on an
/// `O_APPEND` descriptor to that sidecar, from which
/// `ci/nextest-test-results.rs` classifies the test's row as a skip (schema
/// 5) instead of a pass. The line stays under `PIPE_BUF` so parallel test
/// processes cannot interleave entries; a longer entry is a programming
/// error and panics rather than truncate.
pub fn record_test_skip(test: &str, reason: &str) {
    let sidecar = std::env::var_os("HERMIT_TEST_SKIP_FILE").map(PathBuf::from);
    record_test_skip_to(sidecar.as_deref(), test, reason);
}

/// [`record_test_skip`] with the sidecar named by `sidecar` instead of
/// `HERMIT_TEST_SKIP_FILE`.
pub fn record_test_skip_to(sidecar: Option<&Path>, test: &str, reason: &str) {
    println!("HERMIT-TEST-SKIP: {test}\t{reason}");
    let Some(path) = sidecar else {
        return;
    };
    let line = format!("HERMIT-TEST-SKIP: {test}\t{reason}\n");
    assert!(
        line.len() < 4096,
        "skip marker for {test} is {} bytes; keep the reason under one PIPE_BUF line",
        line.len()
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("HERMIT_TEST_SKIP_FILE must name the runner's writable skip sidecar");
    let written = file
        .write(line.as_bytes())
        .expect("writing a skip marker to HERMIT_TEST_SKIP_FILE failed");
    assert_eq!(
        written,
        line.len(),
        "a skip marker must land in one write() so parallel appends cannot interleave"
    );
}
