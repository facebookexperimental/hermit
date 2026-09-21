/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// The Reverie pin reader shared by the two build scripts that need it.
//
// `include!`d rather than imported because both consumers are build scripts in
// different crates (`hermit-cli/build_support.rs` and `hermit-install/build.rs`)
// and cannot depend on each other. Two hand-copied readers previously existed
// with nothing asserting they agreed.
//
// The rule here is the one `scripts/check-reverie-pin.rs::unique_pin` enforces:
// **every** Reverie revision named in the manifest must agree, and a manifest
// naming more than one is refused rather than resolved. First-match-wins is the
// grep hazard this project has been bitten by -- a recursive search returns
// several answers and the true one is not the most common, nor the first.
//
// Two concrete inputs that first-match-wins gets wrong, both reachable in a
// locally-dirty tree mid-bump:
//   * a commented-out dependency line above the live one, whose revision is
//     read as though it were the pin;
//   * a manifest halfway through a bump, naming the old revision on one line
//     and the new one on the next.
//
// Ambiguity yields `None`, which callers render as `"unknown"`. The pin guard
// SKIPS on `"unknown"` rather than refusing, so an unreadable manifest degrades
// to the pre-guard behaviour instead of refusing a correctly staged runtime.
// That is the deliberate choice between the two available errors: failing to
// catch a stale runtime is recoverable, while refusing a good one while naming
// a revision that was never the pin sends the reader somewhere that does not
// exist.

/// Extract the single Reverie revision a manifest pins, or `None` if the
/// manifest names zero revisions, more than one distinct revision, or names
/// Reverie on a line whose revision cannot be parsed.
fn parse_reverie_pin(text: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for line in text.lines() {
        // A commented-out dependency is not the pin. Skipping these is what
        // makes the all-must-agree rule safe to apply to a dirty tree.
        if line.trim_start().starts_with('#') {
            continue;
        }
        if !line.contains("rrnewton/reverie") {
            continue;
        }
        let rev = line
            .split("rev = \"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .filter(|rev| rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()));
        // A Reverie line we cannot parse makes the manifest ambiguous. Treating
        // it as absent would let a malformed line hide a disagreement.
        let rev = rev?;
        match &found {
            Some(seen) if seen != rev => return None,
            Some(_) => {}
            None => found = Some(rev.to_owned()),
        }
    }
    found
}
