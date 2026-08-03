#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Block Hermit changes whose Reverie dependency pin is inconsistent with
//! `rrnewton/reverie:main`.
//!
//! This is a *consistency* check, not a *currency* check: the pin must be an
//! ancestor of the current `main` tip (i.e. a real commit on main's history),
//! but it does NOT have to equal the very latest tip. A pin that is merely
//! behind main is fine and passes; only a pin that is not on main's history at
//! all — a typo, an orphaned SHA, or an unmerged/side-branch commit — is
//! blocked. All Reverie `rev`s across the manifests must still be identical.
//!
//! Local use on Meta hosts:
//!
//! ```text
//! with-proxy ./scripts/check-reverie-pin.rs
//! ```
//!
//! A deliberate exception (e.g. deliberately pinning an unmerged commit) must
//! carry a substantive rationale:
//!
//! ```text
//! with-proxy ./scripts/check-reverie-pin.rs \
//!   --allow-stale-reverie-pin "Depends on unmerged Reverie PR #123 for testing"
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const DEFAULT_REMOTE: &str = "https://github.com/rrnewton/reverie.git";
const MAIN_REF: &str = "refs/heads/main";
const OVERRIDE_ENV: &str = "HERMIT_STALE_REVERIE_PIN_REASON";

#[derive(Default)]
struct Config {
    repo: Option<PathBuf>,
    remote: Option<String>,
    main_sha: Option<String>,
    override_reason: Option<String>,
}

#[derive(Debug)]
struct PinOccurrence {
    path: PathBuf,
    line: usize,
    rev: String,
}

fn usage() -> &'static str {
    "Usage: check-reverie-pin.rs [OPTIONS]\n\
     \n\
     Options:\n\
       --repo PATH                         Hermit checkout (default: git root)\n\
       --reverie-remote URL                Reverie remote to query\n\
       --reverie-main SHA                  Known main tip SHA (skips ls-remote; ancestry still fetched)\n\
       --allow-stale-reverie-pin REASON    Explicit reasoned override\n\
       -h, --help                          Show this help\n\
     \n\
     Environment override: HERMIT_STALE_REVERIE_PIN_REASON=\"reason\""
}

fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_args() -> Result<Config, String> {
    let args: Vec<String> = env::args().collect();
    let mut config = Config::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => config.repo = Some(PathBuf::from(take_value(&args, &mut i, "--repo")?)),
            "--reverie-remote" => {
                config.remote = Some(take_value(&args, &mut i, "--reverie-remote")?)
            }
            "--reverie-main" => {
                config.main_sha = Some(take_value(&args, &mut i, "--reverie-main")?)
            }
            "--allow-stale-reverie-pin" => {
                config.override_reason =
                    Some(take_value(&args, &mut i, "--allow-stale-reverie-pin")?)
            }
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
        i += 1;
    }
    if config.override_reason.is_none() {
        config.override_reason = env::var(OVERRIDE_ENV).ok();
    }
    Ok(config)
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn git_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("could not run git rev-parse: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

fn collect_manifests(dir: &Path, manifests: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|error| format!("could not read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("could not read directory entry: {error}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), ".git" | "target" | "third-party" | "ignored") {
                continue;
            }
            collect_manifests(&path, manifests)?;
        } else if file_type.is_file() && entry.file_name() == "Cargo.toml" {
            manifests.push(path);
        }
    }
    Ok(())
}

fn extract_rev(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    for index in 0..bytes.len().saturating_sub(2) {
        if &bytes[index..index + 3] != b"rev" {
            continue;
        }
        let before_is_key_char = index > 0
            && (bytes[index - 1].is_ascii_alphanumeric()
                || matches!(bytes[index - 1], b'_' | b'-'));
        if before_is_key_char {
            continue;
        }
        let mut cursor = index + 3;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'"') {
            return None;
        }
        cursor += 1;
        let end = bytes[cursor..].iter().position(|byte| *byte == b'"')? + cursor;
        return Some(line[cursor..end].to_string());
    }
    None
}

fn read_pins(root: &Path) -> Result<Vec<PinOccurrence>, String> {
    let mut manifests = Vec::new();
    collect_manifests(root, &mut manifests)?;
    manifests.sort();

    let mut pins = Vec::new();
    for path in manifests {
        let contents = fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        for (line_index, line) in contents.lines().enumerate() {
            if !line.contains("github.com/") || !line.contains("/reverie.git") {
                continue;
            }
            let rev = extract_rev(line).ok_or_else(|| {
                format!(
                    "{}:{} is a Reverie git dependency without a quoted rev",
                    path.display(),
                    line_index + 1
                )
            })?;
            if !is_full_sha(&rev) {
                return Err(format!(
                    "{}:{} has non-40-hex Reverie rev {rev:?}",
                    path.display(),
                    line_index + 1
                ));
            }
            pins.push(PinOccurrence {
                path: path.clone(),
                line: line_index + 1,
                rev,
            });
        }
    }
    if pins.is_empty() {
        return Err("no pinned GitHub Reverie dependencies found in Cargo.toml files".to_string());
    }
    Ok(pins)
}

fn query_main(remote: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(["ls-remote", "--exit-code", remote, MAIN_REF])
        .output()
        .map_err(|error| format!("could not run git ls-remote: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-remote {remote} {MAIN_REF} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let sha = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    if !is_full_sha(&sha) {
        return Err(format!("remote returned invalid main SHA {sha:?}"));
    }
    Ok(sha)
}

/// Relationship of the Hermit pin to the current `reverie:main` tip.
enum PinRelation {
    /// Pin equals the main tip.
    Current,
    /// Pin is an ancestor of main (behind, but on main's history). `behind` is
    /// the commit distance when it could be computed.
    BehindConsistent { behind: Option<u64> },
    /// Pin is NOT reachable from main: a typo, orphaned SHA, or an
    /// unmerged/side-branch commit.
    Diverged,
}

fn git_in(dir: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| format!("could not run git {}: {error}", args.join(" ")))
}

/// Determine the pin's relationship to `main` using a cheap treeless
/// (`--filter=tree:0`) fetch of main's commit graph into a scratch repo — no
/// file contents are transferred, only the commit objects needed for an
/// ancestry test.
fn classify_pin(remote: &str, pin: &str, main: &str) -> Result<PinRelation, String> {
    if pin == main {
        return Ok(PinRelation::Current);
    }
    let dir = env::temp_dir().join(format!("reverie-pin-check-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("could not create scratch dir {}: {error}", dir.display()))?;
    let result = classify_in_dir(&dir, remote, pin, main);
    let _ = fs::remove_dir_all(&dir);
    result
}

fn classify_in_dir(dir: &Path, remote: &str, pin: &str, main: &str) -> Result<PinRelation, String> {
    let init = git_in(dir, &["init", "-q"])?;
    if !init.status.success() {
        return Err(format!(
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr).trim()
        ));
    }
    let fetch = git_in(
        dir,
        &[
            "fetch",
            "--quiet",
            "--filter=tree:0",
            "--no-tags",
            remote,
            main,
        ],
    )?;
    if !fetch.status.success() {
        return Err(format!(
            "git fetch {remote} {main} (commit graph) failed: {}",
            String::from_utf8_lossy(&fetch.stderr).trim()
        ));
    }
    // The pin's commit object is present iff it is reachable from main.
    let pin_commit = format!("{pin}^{{commit}}");
    let present = git_in(dir, &["cat-file", "-e", &pin_commit])?
        .status
        .success();
    if !present {
        return Ok(PinRelation::Diverged);
    }
    // Defensive: confirm ancestry explicitly (true whenever the object is
    // reachable from main, but keeps the intent legible).
    if !git_in(dir, &["merge-base", "--is-ancestor", pin, main])?
        .status
        .success()
    {
        return Ok(PinRelation::Diverged);
    }
    let behind = git_in(dir, &["rev-list", "--count", &format!("{pin}..{main}")])
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        });
    Ok(PinRelation::BehindConsistent { behind })
}

fn validate_reason(reason: &str) -> Result<&str, String> {
    let reason = reason.trim();
    if reason.len() < 20 || reason.split_whitespace().count() < 3 {
        return Err(
            "override rationale must be at least 20 characters and three words".to_string(),
        );
    }
    Ok(reason)
}

fn loud_header(title: &str) {
    eprintln!("======================================================================");
    eprintln!("REVERIE PIN LINT: {title}");
    eprintln!("======================================================================");
}

fn accept_override(reason: Option<&str>) -> bool {
    let Some(reason) = reason else {
        return false;
    };
    let reason = match validate_reason(reason) {
        Ok(reason) => reason,
        Err(error) => {
            eprintln!();
            eprintln!("OVERRIDE REJECTED: {error}");
            return false;
        }
    };
    eprintln!();
    eprintln!("EXPLICIT OVERRIDE ACCEPTED");
    eprintln!("Reason: {reason}");
    eprintln!("This exception is auditable and does not make the pin current.");
    true
}

fn blocked_instructions() {
    eprintln!();
    eprintln!("BLOCKED. Update the pin via docs/updating-reverie.md.");
    eprintln!("A deliberate temporary exception requires a substantive reason:");
    eprintln!("  {OVERRIDE_ENV}=\"why latest main cannot be used\" git commit ...");
    eprintln!("  check-reverie-pin.rs --allow-stale-reverie-pin \"reason\"");
    eprintln!("Preland CI reads: Stale-Reverie-Pin-Reason: <reason> from the PR body.");
}

fn run() -> Result<i32, String> {
    let config = parse_args()?;
    let root = config.repo.clone().map_or_else(git_root, Ok)?;
    let pins = read_pins(&root)?;

    let mut by_rev: BTreeMap<&str, Vec<&PinOccurrence>> = BTreeMap::new();
    for pin in &pins {
        by_rev.entry(&pin.rev).or_default().push(pin);
    }
    if by_rev.len() != 1 {
        loud_header("INCONSISTENT HERMIT REVERIE REVISIONS - BLOCKED");
        for (rev, occurrences) in by_rev {
            eprintln!("  {rev}");
            for occurrence in occurrences {
                let path = occurrence
                    .path
                    .strip_prefix(&root)
                    .unwrap_or(&occurrence.path);
                eprintln!("    {}:{}", path.display(), occurrence.line);
            }
        }
        return Ok(1);
    }

    let pin = pins[0].rev.as_str();
    let remote = config.remote.as_deref().unwrap_or(DEFAULT_REMOTE);
    let main_result = match config.main_sha {
        Some(sha) if is_full_sha(&sha) => Ok(sha),
        Some(sha) => Err(format!(
            "--reverie-main must be 40 hex characters, got {sha:?}"
        )),
        None => query_main(remote),
    };

    let main = match main_result {
        Ok(main) => main,
        Err(error) => {
            loud_header("COULD NOT VERIFY LATEST REVERIE MAIN - BLOCKED");
            eprintln!("Hermit pin: {pin}");
            eprintln!("Lookup error: {error}");
            if accept_override(config.override_reason.as_deref()) {
                return Ok(0);
            }
            blocked_instructions();
            return Ok(1);
        }
    };

    let manifest_count: BTreeSet<&Path> = pins.iter().map(|item| item.path.as_path()).collect();
    let entries = pins.len();
    let manifests = manifest_count.len();

    let relation = match classify_pin(remote, pin, &main) {
        Ok(relation) => relation,
        Err(error) => {
            loud_header("COULD NOT VERIFY REVERIE MAIN RELATIONSHIP - BLOCKED");
            eprintln!("Hermit pin: {pin}");
            eprintln!("Latest main: {main}");
            eprintln!("Lookup error: {error}");
            if accept_override(config.override_reason.as_deref()) {
                return Ok(0);
            }
            blocked_instructions();
            return Ok(1);
        }
    };

    match relation {
        PinRelation::Current => {
            println!(
                "Reverie pin is current: {pin} ({entries} dependency entries across {manifests} manifests)"
            );
            Ok(0)
        }
        PinRelation::BehindConsistent { behind } => {
            let distance = match behind {
                Some(1) => "1 commit behind".to_string(),
                Some(n) => format!("{n} commits behind"),
                None => "behind".to_string(),
            };
            println!(
                "Reverie pin is consistent: {pin} is an ancestor of reverie main ({entries} dependency entries across {manifests} manifests)."
            );
            println!("Latest main: {main} ({distance}). A bump is optional, not required.");
            Ok(0)
        }
        PinRelation::Diverged => {
            loud_header("REVERIE PIN NOT ON MAIN HISTORY - BLOCKED");
            eprintln!("Hermit pin:         {pin}");
            eprintln!("Latest main:        {main}");
            eprintln!("Remote:             {remote}");
            eprintln!("Affected manifests: {manifests}");
            eprintln!("The pinned commit is NOT an ancestor of reverie main.");
            eprintln!("It may be a typo, an orphaned SHA, or an unmerged/side-branch commit.");
            eprintln!("Compare: https://github.com/rrnewton/reverie/compare/{pin}...{main}");
            if accept_override(config.override_reason.as_deref()) {
                return Ok(0);
            }
            blocked_instructions();
            Ok(1)
        }
    }
}

fn main() {
    rust_script_prelude::init();
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            loud_header("CHECKER ERROR - BLOCKED");
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rev_key_not_reverie_prefix() {
        let line = r#"reverie = { git = "https://github.com/rrnewton/reverie.git", rev = "0123456789abcdef0123456789abcdef01234567" }"#;
        assert_eq!(
            extract_rev(line).as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn override_requires_substantive_reason() {
        assert!(validate_reason("temporary").is_err());
        assert!(validate_reason("Waiting for Reverie PR #123 to merge first").is_ok());
    }

    #[test]
    fn validates_full_sha() {
        assert!(is_full_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_sha("01234567"));
        assert!(!is_full_sha("z123456789abcdef0123456789abcdef01234567"));
    }
}
