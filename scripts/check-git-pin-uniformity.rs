#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Refuse a git dependency that is pinned at MORE THAN ONE REVISION.
//!
//! # WHY THIS EXISTS SEPARATELY FROM `check-reverie-pin.rs`
//!
//! `check-reverie-pin.rs` already enforces this invariant -- for Reverie, and
//! only for Reverie. Hermit pins three git dependencies. Measured at
//! `origin/main` on 2026-08-24, none was split:
//!
//! ```text
//!   rrnewton/reverie                 46 entries / 10 files   GATED
//!   rrnewton/liteinst2                2 entries /  2 files   ungated
//!   facebookexperimental/rust-shed    2 entries /  1 file    ungated
//! ```
//!
//! So the defect was never "Reverie is split". It is that Reverie is THE ONLY
//! ONE WATCHED: two of the three have the identical failure mode and no alarm.
//!
//! # WHAT A SPLIT PIN ACTUALLY COSTS
//!
//! If two crates from one dependency resolve to different revisions, a
//! mechanism can be HALF PRESENT. The half you read is there; the half that
//! runs is not. Nothing errors -- the build succeeds, the tests pass, and the
//! verdict is quietly wrong. That is strictly worse than a broken build, and it
//! is not a property of Reverie in any way.
//!
//! # SCOPE IS THE LOAD-BEARING DECISION: TRACKED FILES ONLY
//!
//! ⚠️ This reads `git ls-files` and NEVER walks the directory tree. That choice
//! is not tidiness, it is the whole correctness of the answer, and it was
//! learned the expensive way. A recursive grep for this exact question returned
//! a confident wrong answer, because it swept up 16 `Cargo.toml` files under an
//! untracked `scratch/` directory holding another agent's in-progress pin bump,
//! in a checkout 169 commits behind main. Zero TRACKED files contained the
//! second revision it reported. `check-reverie-pin.rs` scopes to tracked files
//! and was never fooled by the same directory.
//!
//! An untracked working copy is not the repository's pin. A generated or
//! vendored artefact is not the repository's pin. Only tracked dependency
//! metadata and the tracked Buck runtime identity are scanned.
//!
//! # THREE OUTCOMES, NOT TWO
//!
//! - exit 0 -- every REVISION-PINNED git dependency is pinned at exactly one
//!   revision. ⚠️ NOT the same as "every dependency is pinned": a branch- or
//!   tag-tracked dependency is pinned at NO revision, and exit 0 with an
//!   approved floating exception means uniformity was checked over a set that
//!   excludes it. The summary line says so rather than leaving it to a footnote.
//! - exit 1 -- REFUSE: a dependency is split, or floats without an approved
//!   exception, or carries an exception that no longer applies. Every divergent
//!   occurrence is named by file and line.
//! - exit 2 -- COULD-NOT-DETERMINE: the scan itself failed. Distinguished from
//!   a pass so a broken check cannot read as a clean tree, which is this
//!   repository's recurring defect class.

/// Dependencies KNOWN to be branch- or tag-tracked, with the reason.
///
/// ⚠️ THIS LIST IS A RATCHET, NOT A MUTE BUTTON. A floating dependency is
/// strictly worse than one pinned at an old revision: the old revision is at
/// least reproducible, whereas a branch moves under you and the lockfile is the
/// only thing recording what you actually built. So floating REFUSES by
/// default; an entry here is a deliberate, dated exception that must say why.
///
/// It cannot rot, because the gate ALSO refuses an entry that no longer
/// describes reality. If a dependency here becomes revision-pinned, this line
/// must be deleted, and until it is the check fails. A stale exception list
/// that silently permits things is how the original defect got in.
const KNOWN_FLOATING: &[(&str, &str)] = &[(
    "https://github.com/facebookexperimental/rust-shed",
    "branch=main since before 2026-08-24; the manifests are autocargo-generated \
     from Meta's internal Buck targets, so pinning it is an owner decision \
     rather than a hand edit. Only the lockfile records what was built \
     (84a82026 at time of writing). Filed separately.",
)];

const REVERIE_URL: &str = "https://github.com/rrnewton/reverie";
const BUCK_REVERIE_PIN_PATH: &str = "hermit-cli/BUCK";

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

/// What a single line records about a dependency's revision.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Pin {
    /// A concrete 40-hex (or abbreviated) commit.
    Revision(String),
    /// `branch = "main"` or `tag = "..."`: no revision is recorded here at all.
    Floating(String),
}

/// One pin occurrence in tracked Cargo metadata.
#[derive(Clone, Debug)]
struct Occurrence {
    path: PathBuf,
    line: usize,
}

/// `rust_script_prelude::init()` must be main's FIRST statement -- not merely
/// present -- so an early consumer closing the pipe ends the run cleanly
/// instead of panicking on a broken pipe. `check-script-sigpipe.sh` enforces
/// the position, and it caught this script with a comment in the way.
fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("check-git-pin-uniformity.rs: COULD NOT DETERMINE -- {message}");
            eprintln!(
                "  This is NOT a pass. The scan did not complete, so it says nothing \
                 about whether a dependency is split."
            );
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let root = git_root()?;
    let files = tracked_cargo_metadata(&root)?;
    if files.is_empty() {
        return Err("no tracked Cargo metadata files found; refusing to report a clean tree from an empty scan".into());
    }

    // dependency url -> pin -> occurrences
    let mut by_dep: BTreeMap<String, BTreeMap<Pin, Vec<Occurrence>>> = BTreeMap::new();
    for relative in &files {
        let absolute = root.join(relative);
        let contents = std::fs::read_to_string(&absolute)
            .map_err(|error| format!("could not read {}: {error}", relative.display()))?;
        for (index, line) in contents.lines().enumerate() {
            let Some((url, pin)) = extract(line) else {
                continue;
            };
            by_dep
                .entry(url)
                .or_default()
                .entry(pin)
                .or_default()
                .push(Occurrence {
                    path: relative.clone(),
                    line: index + 1,
                });
        }
    }

    // ⚠️ SUBMODULES ARE A SECOND PLACE THE SAME DEPENDENCY GETS PINNED, and until
    // this existed the checker could not see them -- a uniformity checker blind to
    // one of the things it is meant to keep uniform.
    //
    // Found on https://github.com/rrnewton/hermit/pull/2368, which adds a reverie
    // SUBMODULE tracking `branch = buck2` while hermit already pins reverie by
    // revision in 46 Cargo entries. Three references, all disagreeing, and the KVM
    // thread-signal fix in reverie 13cf8bcb was absent from the buck2 branch -- so
    // a Buck2 build would have compiled against a reverie missing it while the
    // Cargo build had it. This checker passed that silently.
    for (path, url, pin) in submodule_pins(&root)? {
        by_dep
            .entry(url)
            .or_default()
            .entry(pin)
            .or_default()
            .push(Occurrence { path, line: 0 });
    }

    // Buck does not run Cargo build scripts, so hermit-cli/BUCK supplies the
    // same guest-visible build identity through HERMIT_REVERIE_PIN. Treat it
    // as another occurrence of the Reverie pin: otherwise Cargo and the
    // submodule can move together while Buck quietly reports the old source.
    let (path, line, revision) = buck_reverie_pin(&root)?;
    by_dep
        .entry(REVERIE_URL.to_string())
        .or_default()
        .entry(Pin::Revision(revision))
        .or_default()
        .push(Occurrence { path, line });

    if by_dep.is_empty() {
        return Err(format!(
            "scanned {} tracked Cargo metadata file(s) and found NO git dependency pins at all. \
             A pin syntax change would look exactly like this, so it is reported as \
             could-not-determine rather than as a clean tree",
            files.len()
        ));
    }

    println!(
        "Scope: {} tracked Cargo metadata file(s) PLUS tracked submodule gitlinks and the Buck runtime pin; \
         {} git dependency/ies pinned. \
         Untracked, generated and vendored copies are EXCLUDED by design.",
        files.len(),
        by_dep.len()
    );

    let mut split = Vec::new();
    let mut floating = Vec::new();
    let mut excepted = Vec::new();
    for (url, pins) in &by_dep {
        let entries: usize = pins.values().map(Vec::len).sum();
        let revisions: Vec<_> = pins
            .keys()
            .filter_map(|pin| match pin {
                Pin::Revision(rev) => Some(rev.clone()),
                Pin::Floating(_) => None,
            })
            .collect();
        let floats: Vec<_> = pins
            .keys()
            .filter_map(|pin| match pin {
                Pin::Floating(what) => Some(what.clone()),
                Pin::Revision(_) => None,
            })
            .collect();

        if revisions.len() > 1 {
            println!(
                "  SPLIT {url}  {} across {} REVISIONS",
                plural(entries),
                revisions.len()
            );
            split.push((url.clone(), pins.clone()));
        } else if let Some(rev) = revisions.first() {
            println!("  OK    {url}  {} @ {}", plural(entries), short(rev));
        }

        // ⚠️ NAMED, NOT COUNTED AS A PASS. A branch- or tag-tracked manifest
        // records no revision, so "uniform" says nothing about it: the lockfile
        // holds the only SHA and regenerating the lock moves the dependency
        // with nothing to compare against. This is not a refusal -- it is a
        // deliberate configuration here -- but it must not read as "pinned and
        // verified", which is exactly what omitting it did.
        if !floats.is_empty() {
            let known = KNOWN_FLOATING.iter().find(|(dep, _)| dep == url);
            match known {
                Some((_, why)) => {
                    println!(
                        "  FLOAT {url}  tracked by {} -- KNOWN EXCEPTION: {why}",
                        floats.join(", ")
                    );
                    excepted.push(url.clone());
                }
                None => {
                    println!(
                        "  FLOAT {url}  tracked by {} -- NOT AN APPROVED EXCEPTION",
                        floats.join(", ")
                    );
                    floating.push((url.clone(), floats.join(", "), pins.clone()));
                }
            }
        }
    }

    // A recorded exception that no longer floats must be REMOVED, not left to
    // quietly permit a future regression.
    let mut stale = Vec::new();
    for (dep, _) in KNOWN_FLOATING {
        let still_floats = by_dep.get(*dep).is_some_and(|pins| {
            pins.keys().any(|pin| matches!(pin, Pin::Floating(_)))
        });
        if !still_floats {
            stale.push(*dep);
        }
    }

    let clean = split.is_empty() && floating.is_empty() && stale.is_empty();
    if clean {
        println!(
            "check-git-pin-uniformity.rs: OK -- every REVISION-PINNED git dependency is pinned at exactly one revision"
        );
        if !excepted.is_empty() {
            println!(
                "  ⚠️ {} dependency/ies are NOT PINNED AT ALL and were excluded from that \
                 statement, by approved exception: {}",
                excepted.len(),
                excepted.join(", ")
            );
            println!(
                "  A BRANCH IS NOT A PIN -- it moves under you. Uniformity cannot be \
                 asserted over a dependency that names one."
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    eprintln!();
    if !split.is_empty() {
        eprintln!(
            "check-git-pin-uniformity.rs: REFUSED -- {} git dependency/ies pinned at more than one revision.",
            split.len()
        );
        eprintln!("  A split pin lets a mechanism be HALF PRESENT: the build succeeds, the tests pass,");
        eprintln!("  and the half that actually runs is the wrong one. Nothing else reports this.");
        eprintln!();
        eprintln!("  ⚠️ IF A SUBMODULE AND A CARGO PIN DISAGREE, THE FIX IS NOT TO DELETE ONE.");
        eprintln!("  Removing the submodule, or dropping the Cargo dependency, silences this");
        eprintln!("  check while changing what one build system compiles. Decide which source");
        eprintln!("  is AUTHORITATIVE and make the other follow it, so both build systems");
        eprintln!("  compile the same revision.");
        for (url, pins) in &split {
            eprintln!();
            eprintln!("  {url}");
            // ⚠️ EVERY occurrence is named, not just a count and not just the
            // odd one out. Whoever fixes this has to edit specific lines, and a
            // gate that says "2 revisions" sends them back to re-derive the
            // list the gate already had.
            for (pin, occurrences) in pins {
                let label = match pin {
                    Pin::Revision(rev) => rev.clone(),
                    Pin::Floating(what) => format!("<{what}>"),
                };
                eprintln!("    {label} ({} occurrence(s))", occurrences.len());
                for occurrence in occurrences {
                    // line 0 marks a submodule gitlink, which has no file line. Printing
                    // "path:0" reads as a file and sends the reader hunting for a line
                    // that does not exist.
                    if occurrence.line == 0 {
                        eprintln!("      {} (SUBMODULE gitlink)", occurrence.path.display());
                    } else {
                        eprintln!("      {}:{}", occurrence.path.display(), occurrence.line);
                    }
                }
            }
        }
    }

    if !floating.is_empty() {
        eprintln!();
        eprintln!(
            "check-git-pin-uniformity.rs: REFUSED -- {} git dependency/ies are branch- or tag-tracked with no approved exception.",
            floating.len()
        );
        eprintln!("  ⚠️ THIS IS WORSE THAN A STALE PIN, NOT BETTER. An old revision is at least");
        eprintln!("  reproducible; a branch moves under you, and the lockfile becomes the only");
        eprintln!("  record of what was actually built. Nothing else in this repository reports it.");
        eprintln!("  Pin it to a revision, or add it to KNOWN_FLOATING with a dated reason.");
        for (url, how, pins) in &floating {
            eprintln!();
            eprintln!("  {url}  ({how})");
            for (pin, occurrences) in pins {
                if let Pin::Floating(what) = pin {
                    eprintln!("    <{what}>");
                    for occurrence in occurrences {
                        // line 0 marks a submodule gitlink, which has no file line. Printing
                        // "path:0" reads as a file and sends the reader hunting for a line
                        // that does not exist.
                        if occurrence.line == 0 {
                            eprintln!("      {} (SUBMODULE gitlink)", occurrence.path.display());
                        } else {
                            eprintln!("      {}:{}", occurrence.path.display(), occurrence.line);
                        }
                    }
                }
            }
        }
    }

    if !stale.is_empty() {
        eprintln!();
        eprintln!(
            "check-git-pin-uniformity.rs: REFUSED -- {} KNOWN_FLOATING entry/ies no longer float.",
            stale.len()
        );
        eprintln!("  Delete them. An exception that outlives the condition it excuses is how a");
        eprintln!("  gate quietly stops gating: the next dependency to float here would inherit");
        eprintln!("  a pass nobody granted it.");
        for dep in &stale {
            eprintln!("    {dep}");
        }
    }
    Ok(ExitCode::from(1))
}


/// Every submodule as a pin on its own URL: the recorded gitlink, or a Floating
/// marker when `.gitmodules` names a branch or tag.
///
/// Read from the INDEX (`git ls-files --stage`, mode 160000) and tracked
/// `.gitmodules`, never from `git submodule status` -- that command reports `-`
/// for a POPULATED submodule inside a linked worktree and quotes the RECORDED
/// sha rather than the actual one. Both facts wrong at once; measured while
/// fixing check-script-sigpipe.sh.
fn submodule_pins(root: &Path) -> Result<Vec<(PathBuf, String, Pin)>, String> {
    let output = Command::new("git")
        .args(["ls-files", "--stage"])
        .current_dir(root)
        .output()
        .map_err(|error| format!("could not list the index: {error}"))?;
    if !output.status.success() {
        return Err("git ls-files --stage failed".into());
    }
    let listing = String::from_utf8(output.stdout)
        .map_err(|error| format!("git printed non-UTF-8: {error}"))?;

    let modules = std::fs::read_to_string(root.join(".gitmodules")).unwrap_or_default();
    let mut url_for: BTreeMap<String, String> = BTreeMap::new();
    let mut float_for: BTreeMap<String, String> = BTreeMap::new();
    let mut current = String::new();
    for line in modules.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("path") {
            if let Some(value) = rest.split('=').nth(1) {
                current = value.trim().to_string();
            }
        } else if let Some(rest) = trimmed.strip_prefix("url") {
            if let Some(value) = rest.split('=').nth(1) {
                url_for.insert(current.clone(), normalise(value.trim()));
            }
        } else {
            for key in ["branch", "tag"] {
                if let Some(rest) = trimmed.strip_prefix(key) {
                    if let Some(value) = rest.split('=').nth(1) {
                        float_for.insert(current.clone(), format!("{key}={}", value.trim()));
                    }
                }
            }
        }
    }

    let mut pins = Vec::new();
    for line in listing.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() != Some("160000") {
            continue;
        }
        let Some(sha) = parts.next() else { continue };
        let Some(path) = line.split('\t').nth(1) else { continue };
        // A gitlink with no .gitmodules entry cannot be attributed to a
        // dependency, so it is skipped rather than guessed at.
        let Some(url) = url_for.get(path) else { continue };
        let pin = match float_for.get(path) {
            Some(what) => Pin::Floating(what.clone()),
            None => Pin::Revision(sha.to_string()),
        };
        pins.push((PathBuf::from(path), url.clone(), pin));
    }
    Ok(pins)
}

/// Read the Reverie revision used to synthesize Hermit's build identity under
/// Buck. The file and key are required and singular so deleting or duplicating
/// the runtime identity cannot silently shrink the check's scope.
fn buck_reverie_pin(root: &Path) -> Result<(PathBuf, usize, String), String> {
    let relative = PathBuf::from(BUCK_REVERIE_PIN_PATH);
    let tracked = Command::new("git")
        .args(["ls-files", "--error-unmatch", "--", BUCK_REVERIE_PIN_PATH])
        .current_dir(root)
        .output()
        .map_err(|error| format!("could not inspect {BUCK_REVERIE_PIN_PATH}: {error}"))?;
    if !tracked.status.success() {
        return Err(format!(
            "{BUCK_REVERIE_PIN_PATH} is not tracked; Buck runtime pin coverage is absent"
        ));
    }

    let contents = std::fs::read_to_string(root.join(&relative))
        .map_err(|error| format!("could not read {}: {error}", relative.display()))?;
    let mut occurrences = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        if let Some(revision) = extract_buck_reverie_pin(line)? {
            occurrences.push((index + 1, revision));
        }
    }
    match occurrences.as_slice() {
        [(line, revision)] => Ok((relative, *line, revision.clone())),
        [] => Err(format!(
            "{BUCK_REVERIE_PIN_PATH} has no HERMIT_REVERIE_PIN; Buck would report an unverified runtime identity"
        )),
        _ => Err(format!(
            "{BUCK_REVERIE_PIN_PATH} has {} HERMIT_REVERIE_PIN entries; expected exactly one",
            occurrences.len()
        )),
    }
}

fn extract_buck_reverie_pin(line: &str) -> Result<Option<String>, String> {
    let Some(rest) = line.trim().strip_prefix("\"HERMIT_REVERIE_PIN\"") else {
        return Ok(None);
    };
    let rest = rest
        .trim_start()
        .strip_prefix(':')
        .ok_or_else(|| "HERMIT_REVERIE_PIN must be followed by ':'".to_string())?
        .trim_start();
    let rest = rest
        .strip_prefix('"')
        .ok_or_else(|| "HERMIT_REVERIE_PIN value must be quoted".to_string())?;
    let end = rest
        .find('"')
        .ok_or_else(|| "HERMIT_REVERIE_PIN value has no closing quote".to_string())?;
    let revision = &rest[..end];
    if revision.len() != 40 || !revision.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err("HERMIT_REVERIE_PIN must be one full 40-hex revision".to_string());
    }
    if rest[end + 1..].trim() != "," {
        return Err("HERMIT_REVERIE_PIN must be a literal dictionary value".to_string());
    }
    Ok(Some(revision.to_ascii_lowercase()))
}

fn plural(count: usize) -> String {
    format!("{count} entr{}", if count == 1 { "y" } else { "ies" })
}

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(12)]
}

/// Pull `(dependency url, revision)` out of one line of Cargo metadata.
///
/// Handles every shape this repository actually uses:
///   - `Cargo.toml`: `foo = { git = "URL", rev = "SHA" }`      -- pinned
///   - `Cargo.toml`: `foo = { git = "URL", branch = "main" }`  -- NOT pinned
///   - `Cargo.lock`: `source = "git+URL?rev=SHA#SHA"`
///   - `Cargo.lock`: `source = "git+URL?branch=main#SHA"`
///
/// ⚠️ THE LOCK SHA IS READ AFTER `#`, NOT FROM `?rev=`. An earlier version of
/// this function required `?rev=` and therefore skipped rust-shed entirely,
/// which is branch-tracked -- so the gate printed OK having silently checked
/// two of the three dependencies. A uniformity gate that omits a dependency
/// without saying so is the inert-mechanism pattern it exists to prevent, and
/// it was caught only by comparing its dependency count against an independent
/// scan. The `#` fragment is what Cargo resolved, whatever the query said.
fn extract(line: &str) -> Option<(String, Pin)> {
    if let Some(start) = line.find("git+") {
        let rest = &line[start + "git+".len()..];
        let end = rest.find(['"', '?', '#'])?;
        let url = normalise(&rest[..end]);
        let hash = rest.find('#')? + 1;
        let rev: String = rest[hash..]
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .collect();
        return (!rev.is_empty()).then_some((url, Pin::Revision(rev)));
    }
    let git_start = line.find("git")?;
    let url = quoted_after(&line[git_start..], "git")?;
    if !url.contains("://") {
        return None;
    }
    let url = normalise(&url);
    if let Some(rev) = quoted_after(line, "rev") {
        if !rev.is_empty() && rev.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some((url, Pin::Revision(rev)));
        }
    }
    // A manifest that names a branch or tag records no revision at all. That is
    // a DIFFERENT state from a pinned one and is reported as itself rather than
    // dropped -- dropping it is what hid rust-shed.
    for floating in ["branch", "tag"] {
        if let Some(name) = quoted_after(line, floating) {
            return Some((url, Pin::Floating(format!("{floating}={name}"))));
        }
    }
    None
}

/// Read the quoted value of `key = "..."` at or after the start of `haystack`.
///
/// Requires the key to stand alone, so `rev` does not also match the `rev` in
/// `default-features` style keys or inside a longer identifier.
fn quoted_after(haystack: &str, key: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(found) = haystack[from..].find(key) {
        let index = from + found;
        let before_ok = index == 0
            || !matches!(haystack.as_bytes()[index - 1], b'_' | b'-')
                && !haystack.as_bytes()[index - 1].is_ascii_alphanumeric();
        let mut cursor = index + key.len();
        let bytes = haystack.as_bytes();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if before_ok && bytes.get(cursor) == Some(&b'=') {
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'"') {
                return None;
            }
            cursor += 1;
            let end = bytes[cursor..].iter().position(|byte| *byte == b'"')? + cursor;
            return Some(haystack[cursor..end].to_string());
        }
        from = index + key.len();
    }
    None
}

/// Collapse the spellings of one remote to a single key.
///
/// ⚠️ WITHOUT THIS THE GATE IS WORSE THAN NOTHING. `.../reverie` and
/// `.../reverie.git` are the same dependency and Cargo writes both -- the
/// manifests use one form and the lockfiles the other. Treating them as
/// distinct would put every revision of a uniform dependency into two buckets
/// of one, so the gate would report OK for a genuinely split pin while looking
/// like it checked.
fn normalise(url: &str) -> String {
    url.trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

fn git_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() {
        return Err("not inside a git repository".into());
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|error| format!("git printed non-UTF-8: {error}"))?;
    Ok(PathBuf::from(text.trim()))
}

/// Tracked Cargo metadata only. See the scope note in the module docs.
fn tracked_cargo_metadata(root: &Path) -> Result<Vec<PathBuf>, String> {
    let output = Command::new("git")
        .args(["ls-files", "-z", "--", "*Cargo.toml", "*Cargo.lock"])
        .current_dir(root)
        .output()
        .map_err(|error| format!("could not run git ls-files: {error}"))?;
    if !output.status.success() {
        return Err("git ls-files failed".into());
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|error| format!("git printed non-UTF-8 path: {error}"))?;
    Ok(text
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_and_lock_spellings_of_one_remote_collapse_together() {
        let manifest = extract(
            r#"reverie-kvm = { version = "0.2.0", git = "https://github.com/rrnewton/reverie.git", rev = "13cf8bcb" }"#,
        )
        .expect("manifest pin");
        let lock = extract(
            r#"source = "git+https://github.com/rrnewton/reverie?rev=13cf8bcb#13cf8bcb""#,
        )
        .expect("lock pin");
        // If these ever disagree the gate silently stops working: a split pin
        // lands in two single-revision buckets and reports OK.
        assert_eq!(manifest.0, lock.0);
        assert_eq!(manifest.1, lock.1);
    }

    #[test]
    fn a_line_without_a_revision_is_not_a_pin() {
        assert!(extract(r#"serde = { version = "1", features = ["derive"] }"#).is_none());
        assert!(extract(r#"path-dep = { path = "../detcore" }"#).is_none());
    }

    #[test]
    fn buck_runtime_pin_is_parsed_as_a_full_revision() {
        assert_eq!(
            extract_buck_reverie_pin(
                r#"    "HERMIT_REVERIE_PIN": "123df7c4c0169006fbfe1f11a1553fe333eac937","#
            ),
            Ok(Some(
                "123df7c4c0169006fbfe1f11a1553fe333eac937".to_string()
            ))
        );
        assert_eq!(extract_buck_reverie_pin("# HERMIT_REVERIE_PIN"), Ok(None));
        assert!(extract_buck_reverie_pin(r#""HERMIT_REVERIE_PIN": "123df7c""#).is_err());
        assert!(
            extract_buck_reverie_pin(
                r#""HERMIT_REVERIE_PIN": "123df7c4c0169006fbfe1f11a1553fe333eac937" + suffix,"#
            )
            .is_err()
        );
    }
}
