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
//! # Which main: `--base-ref`
//!
//! By default "main" is the live `origin/main`. A validation passes
//! `--base-ref SHA` instead, the base its driver recorded at launch (the
//! admission floor, which `make` also hands to `ci/run-reverie-pin-check.sh`):
//! the main a pull request was validated against, or the commit itself when
//! the target is already on main. Against the live ref, re-validating a
//! landed commit failed as soon as main shipped a newer version: the nightly
//! Buck/Cargo reproduction of e6cd3889 (RECORD_VERSION 0x129) was refused
//! after main reached 0x12a, with the same tree and checker that had passed
//! three hours earlier. A named base that does not resolve to a commit, or
//! that is not an ancestor of `HEAD`, is a CHECKER ERROR (rc 2), never a
//! fall-back to `origin/main`.
//!
//! What a recorded base gives up is a version main ships after the validation
//! started. A pull request lands on top of current main, and one that leaves the
//! two constants alone takes main's values when it is rebased. Two pull requests
//! that both bump the version usually conflict on rebase, but only because each
//! bump also adds a changelog comment above `RECORD_VERSION`: two bumps of the
//! constants alone, to the same value, rebase cleanly, and neither this checker
//! nor the compile-time assertion can see that two stream changes now share one
//! version. No automated check catches that case: it rests on the landing
//! convention that whoever lands a version bump confirms by hand that the
//! version is exactly main's plus one.
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

/// Every repository path this checker reads AT THE FLOOR (`--base-ref`), in the
/// pattern forms of `ci/admission/floor-sensitive-paths.txt`: [`versions_on`]
/// reads METADATA at every commit of the floor's history. The `disjoint-main`
/// admission test in `scripts/lib/validate_admission.rs` reads this constant
/// and requires the floor-sensitive list to cover each entry.
///
/// The test `every_git_call_is_registered` is a TRIPWIRE, not a proof that
/// this list is complete. It catches a new, changed or duplicated Git call
/// and any new `Command::new`. It does not catch a pathspec built with
/// `format!` from a variable it cannot see (`&blob` below is one: the
/// registration, not the call text, says it names METADATA), or a widened
/// argument passed through an already registered call. A reviewer of any
/// change to floor reads must check them by hand.
pub const FLOOR_READ_PATHSPECS: &[&str] = &["hermit-cli/src/metadata.rs"];
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

/// `git` run in the repository at `root`. An inherited GIT_DIR or work tree
/// (a hook sets them) would name another repository than `root`.
fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    command
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = git_command(root).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The command line: `--allow-missing-history` and `--base-ref REF`.
#[derive(Debug, Default, PartialEq)]
struct Options {
    allow_missing: bool,
    base_ref: Option<String>,
}

/// Parse the arguments after the program name. An unknown argument, a
/// repeated `--base-ref` or one without a value is an error, so a mistyped
/// flag cannot quietly select the live `origin/main`.
fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--allow-missing-history" => options.allow_missing = true,
            "--base-ref" => {
                let value = args.next().ok_or("--base-ref needs a value")?;
                if value.is_empty() || value.starts_with('-') {
                    return Err(format!("--base-ref needs a ref, got {value:?}"));
                }
                if options.base_ref.replace(value.clone()).is_some() {
                    return Err("--base-ref given twice".into());
                }
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(options)
}

/// The commit a named base ref resolves to, or `None`.
fn resolve_commit(root: &Path, reference: &str) -> Option<String> {
    let spec = format!("{reference}^{{commit}}");
    git(root, &["rev-parse", "--verify", "--quiet", &spec]).map(|sha| sha.trim().to_string())
}

/// Whether `ancestor` is an ancestor of (or equal to) `HEAD`.
fn is_ancestor_of_head(root: &Path, ancestor: &str) -> bool {
    git_command(root)
        .args(["merge-base", "--is-ancestor", ancestor, "HEAD"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Choose the ref whose history stands for "what has shipped".
///
/// With a named base, the answer is that base's commit or an error; the live
/// main is never consulted, so a base that does not resolve, or that is not an
/// ancestor of the tree being checked, cannot quietly become `origin/main`.
/// Without one, it is whatever `live_main` finds, `None` when there is no main.
/// The three lookups are parameters so the choice is testable without git.
fn choose_main_ref(
    base_ref: Option<&str>,
    resolve: impl Fn(&str) -> Option<String>,
    is_ancestor: impl Fn(&str) -> bool,
    live_main: impl FnOnce() -> Option<String>,
) -> Result<Option<String>, String> {
    let Some(base) = base_ref else {
        return Ok(live_main());
    };
    let Some(sha) = resolve(base) else {
        return Err(format!(
            "--base-ref {base} names no commit in this checkout. The validation's\n\
             recorded base is the main this tree is checked against; reading the\n\
             live origin/main instead would answer a different question."
        ));
    };
    if !is_ancestor(&sha) {
        return Err(format!(
            "--base-ref {base} ({sha}) is not an ancestor of HEAD, so its history is\n\
             not the history this tree was built on."
        ));
    }
    Ok(Some(sha))
}

/// The floor the rule requires: the highest version in main's history or in
/// this tree.
fn required_floor(history: &[u32], tree: u32) -> u32 {
    history.iter().copied().chain([tree]).max().unwrap_or(tree)
}

/// The ref standing for "what has shipped". `origin/main` first, because a
/// developer's local `main` can lag or lead.
fn resolve_main_ref(root: &Path) -> Option<String> {
    for candidate in ["origin/main", "main"] {
        if git(root, &["rev-parse", "--verify", "--quiet", candidate]).is_some() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Every distinct `RECORD_VERSION` that has ever appeared in `metadata.rs` on
/// the given ref, newest first. Commits predating the file are skipped rather
/// than treated as zero.
fn versions_on(root: &Path, main_ref: &str) -> Vec<u32> {
    let Some(log) = git(root, &["log", "--format=%H", main_ref, "--", METADATA]) else {
        return Vec::new();
    };
    let mut seen = Vec::new();
    // NB: no let-chains. This file is compiled at --edition=2021 by the CI
    // wrapper, matching ci/run-reverie-pin-check.sh, so it builds on a pristine
    // image with stable rustc and no rust-script.
    for commit in log.split_whitespace() {
        let blob = format!("{commit}:{METADATA}");
        if let Some(source) = git(root, &["show", &blob]) {
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

/// The checker over the repository rooted at `root`, given the arguments
/// after the program name. Returns the exit status.
fn run_in(root: &Path, args: &[String]) -> i32 {
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(error) => {
            loud("CHECKER ERROR - bad arguments");
            eprintln!(
                "{error}\nusage: check-record-version-floor.rs [--allow-missing-history] [--base-ref REF]"
            );
            return 2;
        }
    };
    let allow_missing = options.allow_missing;

    let metadata = root.join(METADATA);
    if !metadata.exists() {
        loud("CHECKER ERROR - metadata.rs not found");
        eprintln!("expected {METADATA} relative to the repository root; run from the root.");
        return 2;
    }
    let source = match std::fs::read_to_string(&metadata) {
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

    let main_ref = match choose_main_ref(
        options.base_ref.as_deref(),
        |reference| resolve_commit(root, reference),
        |sha| is_ancestor_of_head(root, sha),
        || resolve_main_ref(root),
    ) {
        Ok(main_ref) => main_ref,
        Err(error) => {
            loud("CHECKER ERROR - the named base ref cannot stand for main");
            eprintln!("{error}");
            return 2;
        }
    };
    let Some(main_ref) = main_ref else {
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

    let history = versions_on(root, &main_ref);
    if history.is_empty() {
        loud("CHECKER ERROR - no RECORD_VERSION found in history");
        eprintln!(
            "walked {main_ref} for {METADATA} and recovered no version literal.\n\
             A shallow clone or a path rename will do this. Not a pass."
        );
        return 2;
    }

    let required = required_floor(&history, current);
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run_in(Path::new("."), &args));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every Git call outside the tests, as [`source_calls`] spells it, and
    /// the [`FLOOR_READ_PATHSPECS`] entry it reads at the floor (`None`: it
    /// reads no floor path), one entry per call: the tripwire compares
    /// multisets. Unlike check-reverie-pin.rs, which follows the floor's name,
    /// this registers every Git call: there are few, and the floor travels
    /// under several names here (`main_ref`, `reference`, `ancestor`).
    const GIT_CALLS: &[(&str, Option<&str>)] = &[
        // git_command(): the one Command that runs Git.
        (r#"Command::new("git")"#, None),
        // git(): runs its caller's arguments, each registered here.
        ("git_command(root).args(args)", None),
        // resolve_commit(): the floor ref resolves to a commit; no path.
        (
            r#"git(root,&["rev-parse","--verify","--quiet",&spec])"#,
            None,
        ),
        // is_ancestor_of_head(): the floor is an ancestor of HEAD; no path.
        (
            r#"git_command(root).args(["merge-base","--is-ancestor",ancestor,"HEAD"])"#,
            None,
        ),
        // resolve_main_ref(): the live main when no floor is given; no path.
        (
            r#"git(root,&["rev-parse","--verify","--quiet",candidate])"#,
            None,
        ),
        // versions_on(): the floor's history of METADATA ...
        (
            r#"git(root,&["log","--format=%H",main_ref,"--",METADATA])"#,
            Some(METADATA),
        ),
        // ... and METADATA at each of those commits.
        (r#"git(root,&["show",&blob])"#, Some(METADATA)),
    ];

    fn sorted(texts: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut texts: Vec<String> = texts.into_iter().collect();
        texts.sort();
        texts
    }

    /// The Git tripwire over `source`, comparing as multisets so a duplicated
    /// call is caught: every call whose callee's last segment is `git` or
    /// `git_command`, and every `Command::new` under any path prefix, must be
    /// one of GIT_CALLS.
    fn git_tripwire(source: &str) -> Result<(), String> {
        let found = sorted(
            source_calls(source)
                .into_iter()
                .filter(|(callee, _)| {
                    let last = callee.rsplit([':', '.']).next().unwrap_or(callee);
                    matches!(last, "git" | "git_command") || callee.ends_with("Command::new")
                })
                .map(|(_, text)| text),
        );
        let registered = sorted(GIT_CALLS.iter().map(|(text, _)| text.to_string()));
        if found != registered {
            return Err(format!(
                "the Git calls changed; register each in GIT_CALLS with the floor path it \
                 reads: found {found:#?}, registered {registered:#?}"
            ));
        }
        Ok(())
    }

    /// The Git TRIPWIRE over this file (see FLOOR_READ_PATHSPECS for what it
    /// cannot catch): it passes, and the paths the registered calls read at
    /// the floor are exactly FLOOR_READ_PATHSPECS.
    #[test]
    fn every_git_call_is_registered() {
        git_tripwire(include_str!("check-record-version-floor.rs")).unwrap();
        let read: std::collections::BTreeSet<&str> =
            GIT_CALLS.iter().filter_map(|(_, read)| *read).collect();
        let declared: std::collections::BTreeSet<&str> =
            FLOOR_READ_PATHSPECS.iter().copied().collect();
        assert_eq!(read, declared);
        assert_eq!(FLOOR_READ_PATHSPECS, [METADATA]);
    }

    /// This file with `line` added at the top of versions_on.
    fn with_line_in_versions_on(line: &str) -> String {
        let source = include_str!("check-record-version-floor.rs");
        let anchor = "fn versions_on(root: &Path, main_ref: &str) -> Vec<u32> {\n";
        assert_eq!(
            source.matches(anchor).count(),
            1,
            "the versions_on definition"
        );
        source.replacen(anchor, &format!("{anchor}    {line}\n"), 1)
    }

    /// A second copy of a registered Git call is caught.
    #[test]
    fn git_tripwire_counts_duplicate_calls() {
        let duplicated = with_line_in_versions_on(r#"let _ = git(root, &["show", &blob]);"#);
        let e = git_tripwire(&duplicated).unwrap_err();
        assert!(e.contains("Git calls changed"), "{e}");
    }

    /// A Git command that bypasses the registered helpers is caught, under
    /// any path prefix.
    #[test]
    fn git_tripwire_refuses_a_direct_command() {
        for line in [
            r#"let _ = std::process::Command::new("git");"#,
            r#"let _ = self::git(root, &["show", main_ref]);"#,
        ] {
            let e = git_tripwire(&with_line_in_versions_on(line)).unwrap_err();
            assert!(e.contains("Git calls changed"), "{line}: {e}");
        }
    }

    /// Every call expression in this file outside its tests, as (callee, the
    /// call's text with all whitespace removed). A call runs from its callee
    /// through its balanced argument list, plus any chained `.arg(..)` or
    /// `.args(..)`. Macros (`name!(..)`), `fn` signatures and keywords are not
    /// calls. Whole-line comments are dropped, and string, raw-string and
    /// character literals are never read as code.
    fn source_calls(source: &str) -> Vec<(String, String)> {
        let end = source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("the test module marker");
        let code: String = source[..end]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .map(|line| format!("{line}\n"))
            .collect();
        let bytes = code.as_bytes();
        let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        // Mark which bytes are code rather than literal contents.
        let mut is_code = vec![true; bytes.len()];
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    let mut hashes = 0;
                    while i > hashes && bytes[i - 1 - hashes] == b'#' {
                        hashes += 1;
                    }
                    let raw = i > hashes
                        && bytes[i - 1 - hashes] == b'r'
                        && (i == hashes + 1 || !ident(bytes[i - 2 - hashes]));
                    let start = i;
                    i += 1;
                    loop {
                        assert!(i < bytes.len(), "unterminated string literal");
                        if raw {
                            if bytes[i] == b'"'
                                && bytes[i + 1..].iter().take(hashes).all(|b| *b == b'#')
                            {
                                i += 1 + hashes;
                                break;
                            }
                            i += 1;
                        } else if bytes[i] == b'\\' {
                            i += 2;
                        } else if bytes[i] == b'"' {
                            i += 1;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                    for flag in &mut is_code[start..i] {
                        *flag = false;
                    }
                }
                b'\'' if i + 2 < bytes.len() && bytes[i + 1] == b'\\' => {
                    let close = i + 3 + code[i + 3..].find('\'').expect("character literal");
                    for flag in &mut is_code[i..=close] {
                        *flag = false;
                    }
                    i = close + 1;
                }
                b'\'' if i + 2 < bytes.len() && bytes[i + 2] == b'\'' => {
                    for flag in &mut is_code[i..i + 3] {
                        *flag = false;
                    }
                    i += 3;
                }
                _ => i += 1,
            }
        }
        let close_of = |open: usize| -> usize {
            let mut depth = 0usize;
            for (at, byte) in bytes.iter().enumerate().skip(open) {
                if !is_code[at] {
                    continue;
                }
                match byte {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            return at + 1;
                        }
                    }
                    _ => {}
                }
            }
            panic!("unbalanced call at byte {open}")
        };
        let mut calls = Vec::new();
        for open in 1..bytes.len() {
            if bytes[open] != b'(' || !is_code[open] || !ident(bytes[open - 1]) {
                continue;
            }
            let mut start = open;
            while start > 0
                && is_code[start - 1]
                && (ident(bytes[start - 1]) || bytes[start - 1] == b':' || bytes[start - 1] == b'.')
            {
                start -= 1;
            }
            let callee = &code[start..open];
            let bare = callee.rsplit(['.', ':']).next().unwrap_or(callee);
            if matches!(bare, "if" | "while" | "match" | "for" | "return" | "in")
                || code[..start].trim_end().ends_with("fn")
            {
                continue;
            }
            let mut end = close_of(open);
            loop {
                let rest = code[end..].trim_start();
                let skipped = code.len() - end - rest.len();
                let chained = [".args(", ".arg("]
                    .iter()
                    .find(|method| rest.starts_with(**method));
                match chained {
                    Some(method) => end = close_of(end + skipped + method.len() - 1),
                    None => break,
                }
            }
            let text: String = code[start..end]
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            calls.push((
                callee.to_string(),
                text.replace(",]", "]").replace(",)", ")"),
            ));
        }
        calls
    }

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

    // The rule tests below call required_floor, the function run() uses.

    #[test]
    fn the_tree_value_counts_so_the_bump_fails_its_own_pr() {
        // main has shipped up to 0x10f; this PR raises the version and forgets
        // the floor. Against history alone the answer would be 0x10f and the
        // stale floor would pass -- which is hermit#2407 exactly.
        let history = [0x10d, 0x10e, 0x10f];
        assert_eq!(required_floor(&history, 0x110), 0x110);
    }

    /// The regressed-merge tree: the floor must stay at main's value, NOT follow
    /// the tree down. A checker that took the tree's value would demand 0x10a.
    #[test]
    fn a_regressed_tree_does_not_drag_the_floor_down() {
        let history = [0x10d, 0x10e, 0x10f];
        assert_eq!(required_floor(&history, 0x10a), 0x10f);
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn no_arguments_select_the_live_main() {
        assert_eq!(parse_options(&[]), Ok(Options::default()));
    }

    #[test]
    fn a_named_base_is_taken_with_the_history_flag() {
        let sha = "e6cd3889b3896ac14f811bc48897750417a29c0f";
        assert_eq!(
            parse_options(&strings(&["--base-ref", sha, "--allow-missing-history"])),
            Ok(Options {
                allow_missing: true,
                base_ref: Some(sha.into())
            })
        );
    }

    /// Each of these used to be ignored, which ran the check against the live
    /// origin/main while the caller believed it had named a base.
    #[test]
    fn a_malformed_command_line_is_refused_not_ignored() {
        for args in [
            &["--base-ref"][..],
            &["--base-ref", ""],
            &["--base-ref", "--allow-missing-history"],
            &["--base-ref", "a", "--base-ref", "b"],
            &["--base", "a"],
            &["e6cd3889"],
        ] {
            assert!(
                parse_options(&strings(args)).is_err(),
                "{args:?} was accepted"
            );
        }
    }

    #[test]
    fn steady_state_is_the_current_version() {
        let history = [0x100, 0x10e, 0x10f];
        assert_eq!(required_floor(&history, 0x10f), 0x10f);
    }
    const BASE: &str = "e6cd3889b3896ac14f811bc48897750417a29c0f";
    const LIVE: &str = "ec6e01dab1cbf276c498e55dfc8d7c13b44bc54f";

    fn live_main_must_not_be_read() -> Option<String> {
        panic!("a named base must never consult the live main")
    }

    /// The behaviour this change exists for: with a base, its history is the
    /// one walked, and the live main is not read at all.
    #[test]
    fn a_named_base_is_used_instead_of_the_live_main() {
        let chosen = choose_main_ref(
            Some("refs/base"),
            |reference| {
                assert_eq!(reference, "refs/base");
                Some(BASE.to_string())
            },
            |sha| sha == BASE,
            live_main_must_not_be_read,
        );
        assert_eq!(chosen, Ok(Some(BASE.to_string())));
    }

    #[test]
    fn an_unresolvable_base_is_an_error_not_the_live_main() {
        let chosen = choose_main_ref(Some(BASE), |_| None, |_| true, live_main_must_not_be_read);
        assert!(chosen.unwrap_err().contains("names no commit"));
    }

    #[test]
    fn a_base_that_is_not_an_ancestor_of_head_is_an_error() {
        let chosen = choose_main_ref(
            Some(BASE),
            |_| Some(BASE.to_string()),
            |_| false,
            live_main_must_not_be_read,
        );
        assert!(chosen.unwrap_err().contains("not an ancestor of HEAD"));
    }

    #[test]
    fn without_a_base_the_live_main_is_used() {
        let never = |_: &str| -> Option<String> { panic!("no base to resolve") };
        let never_ancestor = |_: &str| -> bool { panic!("no base to check") };
        assert_eq!(
            choose_main_ref(None, never, never_ancestor, || Some(LIVE.to_string())),
            Ok(Some(LIVE.to_string()))
        );
        assert_eq!(
            choose_main_ref(None, never, never_ancestor, || None),
            Ok(None)
        );
    }
    /// A throwaway repository whose main has shipped 0x12a while HEAD is the
    /// earlier 0x129 commit: a landed commit re-validated after main moved.
    /// Returns the repository and the two commits.
    fn moved_main_fixture(name: &str) -> (std::path::PathBuf, String, String) {
        let root = std::env::temp_dir().join(format!(
            "record-version-floor-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("hermit-cli/src")).unwrap();
        let git = |args: &[&str]| -> String {
            let output = git_command(&root)
                .args([
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@example.com",
                ])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .expect("run git");
            assert!(output.status.success(), "git {args:?} failed");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        let commit = |version: u32| -> String {
            std::fs::write(
                root.join(METADATA),
                format!("{VERSION_KEY}{version:#x});\n{FLOOR_KEY}{version:#x};\n"),
            )
            .unwrap();
            git(&["add", METADATA]);
            git(&[
                "commit",
                "-q",
                "-m",
                &format!("record version {version:#x}"),
            ]);
            git(&["rev-parse", "HEAD"])
        };
        git(&["init", "-q"]);
        let old = commit(0x129);
        let new = commit(0x12a);
        git(&["update-ref", "refs/remotes/origin/main", &new]);
        git(&["checkout", "-q", "--detach", &old]);
        (root, old, new)
    }

    /// Through run_in(), the code main() runs: the live main refuses the
    /// landed commit, its own recorded base accepts it, and a base that does
    /// not resolve or is not an ancestor of HEAD is a checker error.
    #[test]
    fn a_landed_commit_is_checked_against_its_recorded_base() {
        let (root, old, new) = moved_main_fixture("base");
        let run = |args: &[&str]| run_in(&root, &strings(args));
        assert_eq!(run(&[]), 1, "the live main has shipped 0x12a");
        assert_eq!(run(&["--base-ref", &old]), 0);
        assert_eq!(
            run(&["--base-ref", &new]),
            2,
            "0x12a's commit is not an ancestor of HEAD"
        );
        assert_eq!(
            run(&["--base-ref", "0000000000000000000000000000000000000001"]),
            2
        );
        assert_eq!(run(&["--base-ref", &old, "--allow-missing-history"]), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Rewrite the fixture tree's two constants, as an uncommitted change.
    fn set_constants(root: &Path, version: u32, floor: u32) {
        std::fs::write(
            root.join(METADATA),
            format!("{VERSION_KEY}{version:#x});\n{FLOOR_KEY}{floor:#x};\n"),
        )
        .unwrap();
    }

    /// Against the recorded base the floor is still enforced both ways: a
    /// version bump that leaves the floor behind, and a floor above anything
    /// ever shipped, are refused (1); only the exact maximum passes.
    #[test]
    fn a_recorded_base_still_refuses_a_floor_that_is_not_the_maximum() {
        let (root, old, _new) = moved_main_fixture("floor");
        let run = || run_in(&root, &strings(&["--base-ref", &old]));
        set_constants(&root, 0x12a, 0x129);
        assert_eq!(
            run(),
            1,
            "the tree bumped to 0x12a and left the floor at 0x129"
        );
        set_constants(&root, 0x129, 0x12b);
        assert_eq!(run(), 1, "0x12b was never shipped and is not in the tree");
        set_constants(&root, 0x12a, 0x12a);
        assert_eq!(run(), 0, "a bump that raises the floor with it");
        let _ = std::fs::remove_dir_all(&root);
    }
}
