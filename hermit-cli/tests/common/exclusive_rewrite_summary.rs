/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/// Check every rewrite outcome, including failed mappings with no scanned pairs.
/// Intentionally left loops and successful rewrite-only rows remain valid.
pub fn check_no_failed_rewrites(summary: &serde_json::Value) -> Result<(), String> {
    let rewrites = summary["exclusive_rewrites"]
        .as_object()
        .ok_or_else(|| "missing exclusive_rewrites object".to_owned())?;
    for (path, entry) in rewrites {
        for field in ["rewritten", "left", "failed"] {
            let count = entry[field]
                .as_u64()
                .ok_or_else(|| format!("{path}: missing unsigned {field}"))?;
            if field == "failed" && count != 0 {
                return Err(format!("{path}: {count} not written"));
            }
        }
    }
    Ok(())
}
