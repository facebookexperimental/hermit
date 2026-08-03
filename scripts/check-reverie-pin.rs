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
//! blocked. All Reverie revisions across tracked Cargo dependency metadata must
//! still be identical.
//!
//! Scope is derived with `git ls-files`: every tracked `Cargo.toml` and
//! `Cargo.lock` is inspected, including tracked vendored paths. Untracked or
//! generated files and files inside nested submodules are outside this check;
//! their contents are not tracked by the Hermit repository.
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
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

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

struct PinScan {
    occurrences: Vec<PinOccurrence>,
    tracked_files: Vec<PathBuf>,
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
     Scope: every tracked Cargo.toml and Cargo.lock from git ls-files.\n\
     Excludes non-Cargo files, untracked/generated files, and nested submodule contents.\n\
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

fn tracked_cargo_metadata(root: &Path) -> Result<Vec<PathBuf>, String> {
    let output = git_in(
        root,
        &[
            "ls-files",
            "-z",
            "--",
            "Cargo.toml",
            "Cargo.lock",
            ":(glob)**/Cargo.toml",
            ":(glob)**/Cargo.lock",
        ],
    )?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files for Cargo dependency metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut paths: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| root.join(path))
        .collect();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Err("git tracks no Cargo.toml or Cargo.lock files".to_string());
    }
    Ok(paths)
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

fn extract_lock_rev(line: &str) -> Option<String> {
    let start = line.find("?rev=")? + "?rev=".len();
    let rev: String = line[start..]
        .chars()
        .take_while(|character| character.is_ascii_hexdigit())
        .collect();
    (!rev.is_empty()).then_some(rev)
}

fn is_reverie_git_source(line: &str) -> bool {
    line.contains("github.com/") && line.contains("/reverie.git")
}

fn read_pins(root: &Path) -> Result<PinScan, String> {
    let tracked_files = tracked_cargo_metadata(root)?;

    let mut pins = Vec::new();
    for path in &tracked_files {
        let contents = fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let file_name = path.file_name().and_then(|name| name.to_str());
        for (line_index, line) in contents.lines().enumerate() {
            if !is_reverie_git_source(line) {
                continue;
            }
            let rev = match file_name {
                Some("Cargo.toml") => extract_rev(line),
                Some("Cargo.lock") => extract_lock_rev(line),
                _ => None,
            }
            .ok_or_else(|| {
                format!(
                    "{}:{} is a Reverie git dependency/source without a pinned rev",
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
                path: path.to_path_buf(),
                line: line_index + 1,
                rev,
            });
        }
    }
    if pins.is_empty() {
        return Err(
            "no pinned GitHub Reverie dependencies found in tracked Cargo.toml/Cargo.lock files"
                .to_string(),
        );
    }
    Ok(PinScan {
        occurrences: pins,
        tracked_files,
    })
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

/// Extract the short-SHA suffix of every LiteInst runtime cache-key token on a
/// line: `liteinst-runtime-build-<hex>` and `liteinst-runtime-<hex>` (6..=40
/// hex digits). Returns the captured short SHAs in order.
///
/// std-only on purpose: CI compiles this file with plain `rustc`
/// (`.github/workflows/ci-portable.yml`), not rust-script/cargo, so no external
/// crate (e.g. `regex`) is available. The nested-workspace path token
/// `liteinst-runtime-build/…` is deliberately NOT matched (it is a directory
/// name, not a revision key): after the optional `-build` the next byte must be
/// `-`, and the hex run must be at least 6 digits, so `-build/…` and the single
/// hex digit in `-build` are both rejected.
fn extract_cache_key_shas(line: &str) -> Vec<String> {
    const MARKER: &str = "liteinst-runtime";
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(rel) = line[from..].find(MARKER) {
        let idx = from + rel;
        let mut cursor = idx + MARKER.len();
        from = cursor;
        if line[cursor..].starts_with("-build") {
            cursor += "-build".len();
        }
        if bytes.get(cursor) != Some(&b'-') {
            continue;
        }
        cursor += 1;
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_hexdigit() {
            cursor += 1;
        }
        let sha = &line[start..cursor];
        if (6..=40).contains(&sha.len()) {
            found.push(sha.to_string());
        }
    }
    found
}

/// Bind every revision-keyed LiteInst runtime build/staging directory to the
/// canonical Reverie pin.
///
/// These directories (`target/liteinst-runtime-build-<short>`,
/// `build_root/liteinst-runtime-<short>`) embed a short prefix of the Reverie
/// pin so the staged runtime cache busts when the pin moves. If a bump updates
/// the Cargo manifests/locks but misses one of these string literals, the stale
/// directory silently reuses a runtime built against the OLD Reverie —
/// `hermit-install/build.rs` carried exactly this drift at `d973a85` after the
/// pin had advanced to `79517704…`. Rather than compare these heterogeneous
/// short forms to each other, bind each to the pin the manifests already agree
/// on: its short SHA MUST be a prefix of the full 40-hex rev. That also makes
/// them mutually consistent (all prefixes of one rev). Hard, offline (no
/// network), and shared by all three enforcement paths (hook, validate.sh, CI)
/// because every consumer already invokes this one checker.
fn check_liteinst_cache_keys(root: &Path, pin: &str) -> Result<i32, String> {
    // Exclude this checker's own source: it embeds deliberately-drifted example
    // tokens in its docstring and test fixtures (a check must not scan the file
    // that defines it). No real revision-keyed cache directory is named here.
    let output = git_in(
        root,
        &[
            "grep",
            "-I",
            "-n",
            "-E",
            "-e",
            r"liteinst-runtime(-build)?-[0-9a-f]{6,40}",
            "--",
            ".",
            ":(exclude,top)scripts/check-reverie-pin.rs",
        ],
    )?;
    // git grep exit codes: 0 = matches, 1 = no matches (fine here), >1 = error.
    match output.status.code() {
        Some(0) | Some(1) => {}
        _ => {
            return Err(format!(
                "git grep for LiteInst cache keys failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    let short = &pin[..7.min(pin.len())];
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut violations: Vec<(String, String)> = Vec::new();
    let mut checked = 0usize;
    for entry in stdout.lines() {
        // `git grep -n` prints `path:line:content`.
        let mut parts = entry.splitn(3, ':');
        let path = parts.next().unwrap_or("");
        let lineno = parts.next().unwrap_or("");
        let content = parts.next().unwrap_or("");
        for sha in extract_cache_key_shas(content) {
            checked += 1;
            if !pin.starts_with(&sha) {
                violations.push((format!("{path}:{lineno}"), sha));
            }
        }
    }
    if !violations.is_empty() {
        loud_header("LITEINST CACHE KEY DRIFT - BLOCKED");
        eprintln!("Canonical Reverie pin: {pin}");
        eprintln!("These revision-keyed LiteInst cache keys are NOT a prefix of the pin:");
        for (location, sha) in &violations {
            eprintln!("  {location}: ...liteinst-runtime[...]-{sha}");
        }
        eprintln!(
            "Update each stale key to the pin's short prefix ({short}) so the staged runtime"
        );
        eprintln!("cache busts when the Reverie pin moves. See docs/updating-reverie.md.");
        return Ok(1);
    }
    eprintln!("LiteInst cache keys: {checked} revision-keyed token(s) all track the pin ({short}).");
    Ok(0)
}

fn run_with_config(config: Config) -> Result<i32, String> {
    let root = config.repo.clone().map_or_else(git_root, Ok)?;
    let scan = read_pins(&root)?;
    let pins = &scan.occurrences;

    let tracked_manifests = scan
        .tracked_files
        .iter()
        .filter(|path| path.file_name().is_some_and(|name| name == "Cargo.toml"))
        .count();
    let tracked_locks = scan.tracked_files.len() - tracked_manifests;
    let pinned_file_count: BTreeSet<&Path> = pins.iter().map(|item| item.path.as_path()).collect();
    eprintln!(
        "Scope: scanned {tracked_manifests} tracked Cargo.toml and {tracked_locks} tracked Cargo.lock files; {} files contain {} Reverie revision entries.",
        pinned_file_count.len(),
        pins.len()
    );
    eprintln!(
        "Scope exclusions: non-Cargo tracked files, untracked/generated files, and nested submodule contents; tracked vendored Cargo metadata is included."
    );

    let mut by_rev: BTreeMap<&str, Vec<&PinOccurrence>> = BTreeMap::new();
    for pin in pins {
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

    // Bind revision-keyed LiteInst runtime cache dirs to the pin. Offline and
    // fast, so run it before the networked ancestry check: cache-key drift
    // fails closed without waiting on the remote.
    let cache_code = check_liteinst_cache_keys(&root, pin)?;
    if cache_code != 0 {
        return Ok(cache_code);
    }

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

    let entries = pins.len();
    let pin_files = pinned_file_count.len();

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
                "Reverie pin is current: {pin} ({entries} revision entries across {pin_files} tracked Cargo metadata files)"
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
                "Reverie pin is consistent: {pin} is an ancestor of reverie main ({entries} revision entries across {pin_files} tracked Cargo metadata files)."
            );
            println!("Latest main: {main} ({distance}). A bump is optional, not required.");
            Ok(0)
        }
        PinRelation::Diverged => {
            loud_header("REVERIE PIN NOT ON MAIN HISTORY - BLOCKED");
            eprintln!("Hermit pin:         {pin}");
            eprintln!("Latest main:        {main}");
            eprintln!("Remote:             {remote}");
            eprintln!("Affected Cargo metadata files: {pin_files}");
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

fn run() -> Result<i32, String> {
    run_with_config(parse_args()?)
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
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

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

    #[test]
    fn help_states_the_checker_scope() {
        let help = usage();
        assert!(help.contains("every tracked Cargo.toml and Cargo.lock"));
        assert!(help.contains("Excludes non-Cargo files"));
    }

    #[test]
    fn extracts_rev_from_lock_source() {
        let line = r#"source = "git+https://github.com/rrnewton/reverie.git?rev=0123456789abcdef0123456789abcdef01234567#0123456789abcdef0123456789abcdef01234567""#;
        assert_eq!(
            extract_lock_rev(line).as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn tracked_stale_lockfile_fails_the_checker_path() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "check-reverie-pin-test-{}-{nonce}",
            std::process::id()
        ));
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).expect("create fixture directories");
        let current = "0123456789abcdef0123456789abcdef01234567";
        let stale = "89abcdef0123456789abcdef0123456789abcdef";
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{current}\" }}\n"
            ),
        )
        .expect("write fixture manifest");
        fs::write(
            runtime.join("Cargo.lock"),
            format!(
                "[[package]]\nname = \"reverie-core\"\nsource = \"git+https://github.com/rrnewton/reverie.git?rev={stale}#{stale}\"\n"
            ),
        )
        .expect("write planted stale lockfile");
        assert!(git_in(&root, &["init", "-q"]).unwrap().status.success());
        assert!(
            git_in(&root, &["add", "Cargo.toml", "runtime/Cargo.lock"])
                .unwrap()
                .status
                .success()
        );

        let scan = read_pins(&root).expect("scan tracked fixture metadata");
        assert_eq!(scan.tracked_files.len(), 2);
        assert!(
            scan.occurrences
                .iter()
                .any(|pin| pin.path.ends_with("runtime/Cargo.lock") && pin.rev == stale)
        );
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            main_sha: Some(current.to_string()),
            ..Config::default()
        })
        .expect("checker should classify the planted inconsistency");
        assert_eq!(code, 1, "a tracked stale Cargo.lock must fail closed");

        fs::remove_dir_all(root).expect("remove fixture repository");
    }

    #[test]
    fn extract_cache_key_shas_handles_both_schemes() {
        assert_eq!(
            extract_cache_key_shas("$PWD/target/liteinst-runtime-build-7951770 arg"),
            vec!["7951770".to_string()]
        );
        assert_eq!(
            extract_cache_key_shas("build_root.join(\"liteinst-runtime-d973a85\")"),
            vec!["d973a85".to_string()]
        );
        // Multiple keys on one line, both schemes.
        assert_eq!(
            extract_cache_key_shas("a/liteinst-runtime-build-7951770 b/liteinst-runtime-abcdef1"),
            vec!["7951770".to_string(), "abcdef1".to_string()]
        );
        // The nested-workspace directory path is NOT a revision key.
        assert!(extract_cache_key_shas("liteinst-runtime-build/Cargo.lock").is_empty());
        // A too-short suffix (<6 hex) is not a revision key.
        assert!(extract_cache_key_shas("liteinst-runtime-ab12").is_empty());
    }

    #[test]
    fn cache_key_drift_fails_and_consistent_passes() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "check-reverie-pin-cachekey-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create fixture directory");
        let pin = "0123456789abcdef0123456789abcdef01234567";
        // Consistent: every cache key is a prefix of the pin.
        fs::write(
            root.join("portable.json"),
            "cmd = target/liteinst-runtime-build-0123456\n",
        )
        .expect("write consistent cache key");
        fs::write(
            root.join("build.rs"),
            "let t = build_root.join(\"liteinst-runtime-0123456789ab\");\n",
        )
        .expect("write consistent cache key");
        assert!(git_in(&root, &["init", "-q"]).unwrap().status.success());
        assert!(
            git_in(&root, &["add", "portable.json", "build.rs"])
                .unwrap()
                .status
                .success()
        );
        assert_eq!(
            check_liteinst_cache_keys(&root, pin).expect("scan consistent tree"),
            0,
            "cache keys that are prefixes of the pin must pass"
        );

        // Drift: plant a key that is not a prefix of the pin.
        fs::write(
            root.join("portable.json"),
            "cmd = target/liteinst-runtime-build-deadbee\n",
        )
        .expect("write drifted cache key");
        assert!(
            git_in(&root, &["add", "portable.json"])
                .unwrap()
                .status
                .success()
        );
        assert_eq!(
            check_liteinst_cache_keys(&root, pin).expect("scan drifted tree"),
            1,
            "a cache key that is not a prefix of the pin must fail closed"
        );

        fs::remove_dir_all(root).expect("remove fixture repository");
    }
}
