// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! Run 2's log from `hermit run --verify --keep-logs`, which Hermit deletes
//! after a match.
//!
//! Hermit creates both runs' logs in the `--verify-log-dir` directory before
//! run 1 starts (`temp_log_files_in` in `hermit-cli/src/bin/hermit/verify.rs`),
//! and run 2's log is written and rewritten in place, never replaced by a new
//! file. After a match Hermit keeps run 1's log, the golden copy, and deletes
//! run 2's (the `keep_golden_log_only` branch in `verify.rs`). A hard link made
//! on any poll while the command runs therefore still holds run 2's complete
//! log once the command exits.

use std::fs;
use std::path::Path;

/// Hard-links the run 2 log listed in `logs`, if there is one, to `capture`.
/// Returns whether the link exists.
pub(super) fn link_run2_log(logs: &Path, capture: &Path) -> bool {
    for entry in fs::read_dir(logs).expect("verify log directory") {
        let entry = entry.expect("verify log entry");
        if !entry.file_name().to_string_lossy().starts_with("run2_log_") {
            continue;
        }
        return match fs::hard_link(entry.path(), capture) {
            Ok(()) => true,
            // Deleted after it was listed; no later poll can see it either.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => panic!("capture {}: {error}", entry.path().display()),
        };
    }
    false
}
