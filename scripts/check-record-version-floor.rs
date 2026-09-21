#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Keep `HIGHEST_SHIPPED_RECORD_VERSION` equal to the highest `RECORD_VERSION`
//! this project has ever shipped, DERIVED FROM HISTORY rather than remembered.
//!
//! # What the floor is for, and why it went stale
//!
//! `hermit-cli/src/metadata.rs` carries two constants and a compile-time
//! assertion over them:
//!
//! ```ignore
//! pub(crate) const RECORD_VERSION: RecordVersion = RecordVersion(0x10f);
//! const HIGHEST_SHIPPED_RECORD_VERSION: u32 = 0x10f;
//! const _: () = assert!(RECORD_VERSION.0 >= HIGHEST_SHIPPED_RECORD_VERSION, "...");
//! ```
//!
//! The floor exists to refuse a `RECORD_VERSION` that has moved BACKWARD — the
//! shape a long-stale branch produces the moment its conflict is resolved by
//! taking the branch side, which would stamp the current schema with a label an
//! older reader already claims.
//!
//! It is an enumerated value sitting beside a moving one, so it can be left
//! behind by a bump — and it was. hermit#2407 raised `RECORD_VERSION`
//! `0x10e -> 0x10f`; the floor stayed at `0x10e` and nothing went red, because
//! `0x10f >= 0x10e` holds. hermit#2586 raised it by hand. This checker exists so
//! the NEXT bump cannot repeat that: it is the difference between fixing an
//! instance and removing the class.
//!
//! `docs/PR_SWEEP_VERDICTS.md` section 17 predicted this and prescribed the
//! remedy: "a CI assertion that the literal equals that maximum makes the floor
//! travel with the constant instead of with someone's memory."
//!
//! # ⚠️ WHY THE FLOOR CANNOT BE DERIVED IN-CRATE, WHICH IS THE OBVIOUS FIX
//!
//! The tempting change is to delete the literal and compute the floor from
//! `RECORD_VERSION`. That makes the guard VACUOUS: `assert!(RECORD_VERSION.0 >=
//! f(RECORD_VERSION.0))` re-derives from whatever the constant currently says,
//! so a regressed constant drags its own floor down with it and the assertion
//! still holds. The same objection sinks a checker that compares the floor to
//! the value in the working tree: on exactly the tree this guard exists to
//! catch — `RECORD_VERSION` regressed to `0x10a`, floor still holding main's
//! `0x10f` — such a check would report the floor "too high" and instruct the
//! author to lower it to `0x10a`, destroying the guard while reporting success.
//!
//! THE ONLY VALID SOURCE OF TRUTH IS HISTORY. "Has shipped" is a fact about
//! `main`, not about the tree in front of you, and it is not available to the
//! compiler. So the literal STAYS — it is what the compile-time assertion needs,
//! and keeping it in a separate hunk from `RECORD_VERSION` is precisely what
//! lets a merge take main's floor while taking a branch's version. This checker
//! supplies the missing half: the literal may no longer go stale.
//!
//! # The rule
//!
//! ```text
//! HIGHEST_SHIPPED_RECORD_VERSION == max(every RECORD_VERSION ever on main,
//!                                       RECORD_VERSION in this tree)
//! ```
//!
//! Including the working tree's value is what makes this fire on the PR that
//! raises `RECORD_VERSION` rather than one commit too late. Against main history
//! alone, the bump that outran the floor would pass its own pull request and
//! only fail afterwards — which is the failure this checker is named for.
//!
//! # Three outcomes, not two
//!
//! PASS (0), REFUSE (1), CHECKER ERROR (2). An unevaluated check is
//! indistinguishable from a passing one, so every could-not-determine FAILS
//! CLOSED:
//!
//!   * either constant missing or unparseable  -> rc 2
//!   * the main ref cannot be resolved (a depth-1 CI clone has no `origin/main`)
//!         -> rc 2, unless `--allow-missing-history` is passed
//!   * the history walk yields no `RECORD_VERSION` at all -> rc 2
//!
//! `--allow-missing-history` exists so a caller with genuinely no history
//! DECLARES it by name instead of stumbling into a silent pass. It prints a
//! loud SKIPPED banner and does not pretend the derivation ran.
//!
//! # What this does NOT assert
//!
//! Two separate properties are in play and this checker covers only the first:
//!
//!   1. the constant cannot move BACKWARD    -- the compile-time assertion,
//!      kept as-is; this checker keeps its floor honest.
//!   2. a SUPERSEDED STREAM cannot be ACCEPTED -- a different property, about
//!      `compatible_with` at read time, asserted by the derived window in
//!      `record_version_requires_an_exact_match`. Nothing here touches it, and
//!      raising the floor never establishes it. Keep both.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::path::Path;
use std::process::Command;

const METADATA: &str = "hermit-cli/src/metadata.rs";
const VERSION_KEY: &str = "const RECORD_VERSION: RecordVersion = RecordVersion(";
const FLOOR_KEY: &str = "const HIGHEST_SHIPPED_RECORD_VERSION: u32 = ";

/// Pull a `0x...` literal out of the line introduced by `key`.
///
/// Deliberately returns `None` rather than a default: a metadata.rs whose
/// constants cannot be found is a could-not-determine, not a pass.
fn parse_after(source: &str, key: &str) -> Option<u32> {
    let line = source.lines().find(|line| line.contains(key))?;
    let rest = line.split(key).nth(1)?;
    let hex = rest.trim_start().strip_prefix("0x")?;
    let digits: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    if digits.is_empty() {
        return None;
    }
    u32::from_str_radix(&digits, 16).ok()
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The ref standing for "what has shipped". `origin/main` first, because a
/// developer's local `main` can lag or lead.
fn resolve_main_ref() -> Option<String> {
    for candidate in ["origin/main", "main"] {
        if git(&["rev-parse", "--verify", "--quiet", candidate]).is_some() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Every distinct `RECORD_VERSION` that has ever appeared in `metadata.rs` on
/// the given ref, newest first. Commits predating the file are skipped rather
/// than treated as zero.
fn versions_on(main_ref: &str) -> Vec<u32> {
    let Some(log) = git(&["log", "--format=%H", main_ref, "--", METADATA]) else {
        return Vec::new();
    };
    let mut seen = Vec::new();
    // NB: no let-chains. This file is compiled at --edition=2021 by the CI
    // wrapper, matching ci/run-reverie-pin-check.sh, so it builds on a pristine
    // image with stable rustc and no rust-script.
    for commit in log.split_whitespace() {
        let blob = format!("{commit}:{METADATA}");
        if let Some(source) = git(&["show", &blob]) {
            if let Some(version) = parse_after(&source, VERSION_KEY) {
                if !seen.contains(&version) {
                    seen.push(version);
                }
            }
        }
    }
    seen
}

fn loud(title: &str) {
    eprintln!("\n=== {title} ===");
}

fn run() -> i32 {
    let allow_missing = std::env::args().any(|a| a == "--allow-missing-history");

    if !Path::new(METADATA).exists() {
        loud("CHECKER ERROR - metadata.rs not found");
        eprintln!("expected {METADATA} relative to the repository root; run from the root.");
        return 2;
    }
    let source = match std::fs::read_to_string(METADATA) {
        Ok(s) => s,
        Err(e) => {
            loud("CHECKER ERROR - metadata.rs unreadable");
            eprintln!("{e}");
            return 2;
        }
    };

    let Some(current) = parse_after(&source, VERSION_KEY) else {
        loud("CHECKER ERROR - RECORD_VERSION not found");
        eprintln!("no line containing `{VERSION_KEY}` with a 0x literal.");
        return 2;
    };
    let Some(floor) = parse_after(&source, FLOOR_KEY) else {
        loud("CHECKER ERROR - HIGHEST_SHIPPED_RECORD_VERSION not found");
        eprintln!(
            "no line containing `{FLOOR_KEY}` with a 0x literal.\n\
             If the floor was deliberately removed, this checker must be removed in the same \
             commit -- an absent floor is not a passing one."
        );
        return 2;
    };

    let Some(main_ref) = resolve_main_ref() else {
        if allow_missing {
            loud("SKIPPED - no main ref, and the caller declared it");
            eprintln!(
                "--allow-missing-history was passed, so the DERIVATION DID NOT RUN.\n\
                 in-tree only: RECORD_VERSION={current:#x} HIGHEST_SHIPPED={floor:#x}\n\
                 This is not evidence the floor is current."
            );
            return 0;
        }
        loud("CHECKER ERROR - cannot resolve origin/main or main");
        eprintln!(
            "\"has shipped\" is a fact about main and cannot be read from this tree.\n\
             A depth-1 CI clone has no origin/main; fetch history, or pass\n\
             --allow-missing-history to DECLARE that the derivation is being skipped."
        );
        return 2;
    };

    let history = versions_on(&main_ref);
    if history.is_empty() {
        loud("CHECKER ERROR - no RECORD_VERSION found in history");
        eprintln!(
            "walked {main_ref} for {METADATA} and recovered no version literal.\n\
             A shallow clone or a path rename will do this. Not a pass."
        );
        return 2;
    }

    let required = history.iter().copied().chain([current]).max().unwrap();
    let mut sorted = history.clone();
    sorted.sort_unstable();

    if floor == required {
        println!(
            "record-version floor OK: HIGHEST_SHIPPED_RECORD_VERSION = {floor:#x} \
             = max(history, tree)"
        );
        println!(
            "  RECORD_VERSION (tree) = {current:#x}; {} distinct versions on {main_ref}: {}",
            sorted.len(),
            sorted
                .iter()
                .map(|v| format!("{v:#x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        return 0;
    }

    loud("REFUSED - HIGHEST_SHIPPED_RECORD_VERSION is not the highest shipped version");
    eprintln!("  HIGHEST_SHIPPED_RECORD_VERSION = {floor:#x}");
    eprintln!("  required                       = {required:#x}");
    eprintln!("  RECORD_VERSION in this tree    = {current:#x}");
    eprintln!(
        "  distinct versions on {main_ref}   = {}",
        sorted
            .iter()
            .map(|v| format!("{v:#x}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    eprintln!();
    if floor < required {
        eprintln!(
            "The floor was left behind. Raise it to {required:#x} in {METADATA}, IN THE SAME\n\
             COMMIT that raised RECORD_VERSION. Until then the compile-time assertion refuses\n\
             only a regression BELOW {floor:#x}, so a move back to {floor:#x} -- a version that\n\
             has already shipped and whose stream shape differs -- would compile clean."
        );
    } else {
        eprintln!(
            "The floor is ABOVE anything ever shipped. Either RECORD_VERSION was regressed --\n\
             which is exactly what the compile-time assertion exists to refuse, so fix the\n\
             version and not the floor -- or the floor was raised past a version that never\n\
             landed. DO NOT lower the floor to silence this."
        );
    }
    1
}

fn main() {
    rust_script_prelude::init();
    std::process::exit(run());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_constants() {
        let src = "\
pub(crate) const RECORD_VERSION: RecordVersion = RecordVersion(0x10f);
const HIGHEST_SHIPPED_RECORD_VERSION: u32 = 0x10e;
";
        assert_eq!(parse_after(src, VERSION_KEY), Some(0x10f));
        assert_eq!(parse_after(src, FLOOR_KEY), Some(0x10e));
    }

    /// A missing constant must be `None` so the caller can fail closed. Returning
    /// a default here is how a checker starts passing on a file it cannot read.
    #[test]
    fn a_missing_constant_is_not_a_zero() {
        assert_eq!(parse_after("nothing here", VERSION_KEY), None);
        assert_eq!(parse_after(VERSION_KEY, VERSION_KEY), None);
        assert_eq!(
            parse_after(
                "const HIGHEST_SHIPPED_RECORD_VERSION: u32 = 271;",
                FLOOR_KEY
            ),
            None,
            "a decimal literal is not the form this file uses; refuse rather than guess"
        );
    }

    /// The rule itself, stated as the property the checker enforces.
    fn required(history: &[u32], tree: u32) -> u32 {
        history.iter().copied().chain([tree]).max().unwrap()
    }

    #[test]
    fn the_tree_value_counts_so_the_bump_fails_its_own_pr() {
        // main has shipped up to 0x10f; this PR raises the version and forgets
        // the floor. Against history alone the answer would be 0x10f and the
        // stale floor would pass -- which is hermit#2407 exactly.
        let history = [0x10d, 0x10e, 0x10f];
        assert_eq!(required(&history, 0x110), 0x110);
    }

    /// The regressed-merge tree: the floor must stay at main's value, NOT follow
    /// the tree down. A checker that took the tree's value would demand 0x10a.
    #[test]
    fn a_regressed_tree_does_not_drag_the_floor_down() {
        let history = [0x10d, 0x10e, 0x10f];
        assert_eq!(required(&history, 0x10a), 0x10f);
    }

    #[test]
    fn steady_state_is_the_current_version() {
        let history = [0x100, 0x10e, 0x10f];
        assert_eq!(required(&history, 0x10f), 0x10f);
    }
}
