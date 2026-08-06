#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Judge Hermit's recorded Reverie pin by ANCESTRY and MONOTONICITY.
//!
//! OWNER-APPROVED RULE, 2026-08-08, replacing equality-to-the-tip:
//!
//!   1. ANCESTRY   -- the pin must be an ancestor of `rrnewton/reverie:main`,
//!                    not equal to its tip. Lagging is legitimate; a pin that
//!                    must equal the tip is not a pin, and made the verdict a
//!                    property of WHEN you looked rather than of the tree.
//!   2. MONOTONIC  -- the pin may only advance. Ancestry ALONE would accept a
//!                    pin walked backwards, because an ancient commit is also
//!                    an ancestor.
//!   3. CONFLICTS TAKE THE NEWER PIN -- enforced BY rule 2 rather than by a
//!                    separate mechanism: resolving a Cargo.toml/Cargo.lock
//!                    conflict to the older side regresses the pin below the
//!                    base, which rule 2 refuses. Conflict resolution is
//!                    exactly where a silent regression would otherwise land.
//!
//! All Reverie revisions across tracked Cargo dependency metadata must also be
//! identical to each other; that is decided offline and always blocks.
//!
//! # THREE OUTCOMES, NOT TWO -- specified deliberately, not left to control flow
//!
//! Every gate has PASS, REFUSE, and COULD-NOT-DETERMINE. Collapsing the third
//! into either of the first two is the defect that produced nearly every gate
//! failure this repository saw on 2026-08-08. THIS GATE FAILS CLOSED on every
//! could-not-determine, and each one is enumerated here so a future edit cannot
//! quietly add a silent-open path:
//!
//!   * the Reverie graph cannot be fetched (network, proxy, bad remote)
//!         -> CHECKER ERROR rc=2. Never PASS: a gate that opens when the
//!            network hiccups is not a gate.
//!   * the fetched graph does not contain `main`
//!         -> CHECKER ERROR rc=2, saying "incomplete fetch, not a pin
//!            violation". Distinct from the pin being off-history.
//!   * `ls-remote` cannot resolve the authority tip
//!         -> BLOCKED rc=1 (pre-existing behaviour, kept).
//!   * the monotonicity BASE cannot be resolved (no such ref, a depth-1 clone
//!     with no `origin/main`, an incoherent base pinning two revisions)
//!         -> CHECKER ERROR rc=2 unless `--no-base` is passed. An unevaluated
//!            monotonicity check is INDISTINGUISHABLE from a passing one, and
//!            it shipped exactly that way: with no base ref the gate printed
//!            "does not regress" and returned 0. `--no-base` exists so a caller
//!            with genuinely no base DECLARES it instead of stumbling into it.
//!
//! The only could-not-determine that is allowed to pass is the one the caller
//! explicitly asked for by name.
//!
//! Scope is derived with `git ls-files`: every tracked `Cargo.toml` and
//! `Cargo.lock` is inspected, including tracked vendored paths. Untracked or
//! generated files and files inside nested submodules are outside this check;
//! their contents are not tracked by the Hermit repository.
//!
//! Every reported pin carries the commit it was read from. The checker reads
//! the *working tree*, so in a checkout that sits behind `main` it faithfully
//! reports the pin of an older commit — which then reads as a stale pin when
//! compared against live Reverie `main`. A bare pin value records none of that,
//! so the reported pin is always accompanied on stderr by its HEAD, plus a loud
//! warning when HEAD is a strict ancestor of `origin/main`.
//!
//! Local use on Meta hosts:
//!
//! ```text
//! with-proxy ./ci/run-reverie-pin-check.sh
//! ```
//!
//! Repair every derived manifest and lockfile site with one command:
//!
//! ```text
//! with-proxy ./ci/run-reverie-pin-check.sh --update-to-latest
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
const DEFAULT_BASE_REF: &str = "origin/main";
struct Config {
    repo: Option<PathBuf>,
    #[cfg(test)]
    remote: Option<String>,
    print_pin: bool,
    update_to_latest: bool,
    /// Skip every NETWORKED judgement (ancestry, monotonicity, and the
    /// main-tip query) and decide only what is decidable offline: that the
    /// tracked manifests agree with each other, and that the LiteInst cache
    /// keys track the pin. Used by the pre-commit hook, which the owner has
    /// ruled must not be a hard blocker on pin currency.
    offline: bool,
    /// Pre-commit advisory. Judges the STAGED pin against HEAD's and against
    /// Reverie master, and speaks in exactly one of four cases (see
    /// `staged_pin_advisory`). Never a hard refusal: case 3 is an
    /// ACKNOWLEDGEMENT, cleared by HERMIT_PIN_BELOW_MASTER_ACK=1.
    staged_advisory: bool,
    /// Declare that this invocation has NO monotonicity base, so an
    /// unresolvable base is an intended skip rather than a silent one.
    no_base: bool,
    /// Revision whose recorded pin is the monotonicity floor. Defaults to
    /// `origin/main`: the base a PR would land on. A caller with no such ref
    /// (a fresh clone, an isolated fixture) simply gets no floor asserted.
    base_ref: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repo: None,
            #[cfg(test)]
            remote: None,
            print_pin: false,
            update_to_latest: false,
            offline: false,
            staged_advisory: false,
            no_base: false,
            base_ref: DEFAULT_BASE_REF.to_string(),
        }
    }
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
       --print-pin                         Print the single locally recorded pin; no network\n\
       --update-to-latest                  Update every derived Cargo pin site to latest main\n\
       --base-ref REF                      Monotonicity floor (default: origin/main)\n\
       --offline                           Local consistency only; no network, no currency\n\
       --no-base                           Declare there is no monotonicity base (skip it)\n\
       --staged-pin-advisory               Pre-commit advisory on a STAGED pin edit\n\
       -h, --help                          Show this help\n\
     \n\
     Scope: every tracked Cargo.toml and Cargo.lock from git ls-files.\n\
     Excludes non-Cargo files, untracked/generated files, and nested submodule contents."
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
            "--print-pin" => config.print_pin = true,
            "--base-ref" => config.base_ref = take_value(&args, &mut i, "--base-ref")?,
            "--offline" => config.offline = true,
            "--no-base" => config.no_base = true,
            "--staged-pin-advisory" => config.staged_advisory = true,
            "--update-to-latest" => config.update_to_latest = true,
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
        i += 1;
    }
    if config.print_pin && config.update_to_latest {
        return Err("--print-pin and --update-to-latest are mutually exclusive".to_string());
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

/// Which commit the pin was read from, so a reported pin carries its conditions.
///
/// A bare pin value is a proxy: it does not record the tree it came from, so a
/// checkout sitting behind `main` yields a perfectly correct pin for an old
/// commit that reads as a stale pin on `main`.
///
/// Offline by construction — only local refs are dereferenced, so `--print-pin`
/// keeps its documented no-network contract. A stale local `origin/main` weakens
/// the signal but cannot make it wrong in the dangerous direction: being a
/// strict ancestor of even a stale `origin/main` still means being behind.
struct CheckoutProvenance {
    head: String,
    /// `Some(main)` only when HEAD is a *strict ancestor* of local `origin/main`.
    ///
    /// Strict ancestry, not inequality: a PR head legitimately differs from
    /// `main` while carrying its own commits, and must not be warned about.
    behind_main: Option<String>,
}

fn rev_parse(root: &Path, rev: &str) -> Option<String> {
    let output = git_in(root, &["rev-parse", "--verify", "--quiet", rev]).ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    is_full_sha(&value).then_some(value)
}

fn checkout_provenance(root: &Path) -> Option<CheckoutProvenance> {
    let head = rev_parse(root, "HEAD")?;
    let Some(main) = rev_parse(root, "refs/remotes/origin/main") else {
        return Some(CheckoutProvenance {
            head,
            behind_main: None,
        });
    };
    let behind = head != main
        && git_in(root, &["merge-base", "--is-ancestor", &head, &main])
            .is_ok_and(|output| output.status.success());
    Some(CheckoutProvenance {
        head,
        behind_main: behind.then_some(main),
    })
}

/// Emit the pin's provenance on stderr. Never touches stdout: `--print-pin`
/// consumers capture stdout by command substitution and must keep receiving
/// exactly the bare pin.
fn report_provenance(provenance: Option<&CheckoutProvenance>) {
    let Some(provenance) = provenance else {
        eprintln!(
            "Pin provenance: HEAD could not be resolved; the reported pin is not bound to a commit."
        );
        return;
    };
    eprintln!("Pin read from checkout HEAD {}.", provenance.head);
    let Some(main) = &provenance.behind_main else {
        return;
    };
    loud_header("CHECKOUT IS BEHIND origin/main - PIN VALUE IS HISTORICAL");
    eprintln!("HEAD         {}", provenance.head);
    eprintln!("origin/main  {main}");
    eprintln!("HEAD is a strict ancestor of origin/main, so the pin reported here is the pin AT");
    eprintln!("THAT OLDER COMMIT -- not the pin on main. Comparing it against live Reverie main");
    eprintln!("will show a spurious 'stale pin' and can trigger a bump that is not needed.");
    eprintln!("Fast-forward this checkout, or read the pin from main without checking it out:");
    eprintln!("  git show origin/main:detcore/Cargo.toml");
    eprintln!("This is a warning, not a refusal: the exit code is unchanged.");
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

/// Materialize enough of Reverie's COMMIT GRAPH to answer ancestry, and return
/// the bare repository holding it.
///
/// `git ls-remote` returns a tip and nothing else, so it can answer "is the pin
/// EQUAL to main" and no other question. Ancestry and monotonicity are both
/// reachability questions, so they need the graph. A blobless bare fetch of the
/// single branch is the cheap way to get one: measured 2026-08-08 against
/// rrnewton/reverie at 1 second and 1.3 MB, inside this node's 120s timeout and
/// its 5s estimate. Cargo's git db also has the graph, but Preflight runs before
/// any cargo fetch, so depending on it would be order-dependent.
///
/// The cache is reused across invocations and re-fetched every time: a stale
/// cache would silently answer with an old main, which is the failure mode this
/// whole change exists to remove.
fn reverie_graph(root: &Path, remote: &str) -> Result<PathBuf, String> {
    let cache = root.join("target/ci/reverie-graph.git");
    if !cache.join("HEAD").is_file() {
        fs::create_dir_all(cache.parent().unwrap_or(&cache))
            .map_err(|error| format!("could not create the Reverie graph cache: {error}"))?;
        let init = Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&cache)
            .output()
            .map_err(|error| format!("could not run git init: {error}"))?;
        if !init.status.success() {
            return Err(format!(
                "git init --bare failed: {}",
                String::from_utf8_lossy(&init.stderr).trim()
            ));
        }
    }
    // `--filter=blob:none` is a BANDWIDTH optimization, not a correctness
    // requirement: ancestry needs commits, never blobs. It is also not
    // universally supported -- a local-PATH remote rejects it outright
    // ("promisor remote name cannot begin with '/'", then a missing-blob
    // fatal), which is exactly how the fixture suite exercises this code. So
    // try filtered first for the real remote, and fall back to a plain fetch
    // rather than letting a transport limitation read as a pin violation.
    // NO `--filter`. A blob-filtered fetch is smaller (1.3 MB vs 12 MB) but it
    // is a SECOND FAILURE MODE for 0.4s of saving: a local-path remote rejects
    // it outright, and a failed attempt writes promisor configuration that then
    // poisons any retry. Both were observed -- the promisor poisoning as an
    // intermittent bracket failure. Measured unfiltered: 1.4s / 12 MB, far
    // inside this node's 120s timeout. A gate should buy robustness with that.
    let fetch = git_in(
        &cache,
        &[
            "fetch",
            "--no-tags",
            "--quiet",
            "--force",
            remote,
            "+refs/heads/main:refs/heads/main",
        ],
    )?;
    if !fetch.status.success() {
        return Err(format!(
            "could not fetch the Reverie commit graph from {remote}: {}",
            String::from_utf8_lossy(&fetch.stderr).trim()
        ));
    }
    Ok(cache)
}

/// Is `ancestor` reachable from `descendant`?
///
/// REACHABILITY, NOT PRESENCE. A blobless fetch of one branch also lands objects
/// that are NOT reachable from it -- measured: after fetching only `main`,
/// `git cat-file -t 88363a56` (a commit that lives solely on an abandoned,
/// later-rebased feature branch) SUCCEEDS. So an object-presence test would
/// wrongly ACCEPT a pin that is not on Reverie's history, which is exactly the
/// case this predicate has to refuse. Only `merge-base --is-ancestor` answers it.
/// ABSENT ALSO MEANS "NOT AN ANCESTOR", and it must be answered rather than
/// raised. Two distinct real-world shapes reach here for an off-history pin:
///   * the commit is PRESENT in the pack but unreachable from main (measured:
///     88363a56, which lives only on an abandoned, later-rebased branch), and
///   * the commit is ABSENT entirely, where `merge-base` exits non-zero with
///     "fatal: Not a valid commit name".
/// Treating the second as an ERROR would turn a genuine violation into a
/// checker crash; treating it as "not reachable" is both true and fail-closed.
/// Anything else is still a real error and is still raised.
fn is_ancestor(graph: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    // ABSENT ANCESTOR is a genuine verdict: the graph has main, and the pin is
    // not in it, so the pin is not reachable. ABSENT DESCENDANT is NOT a
    // verdict -- it means the graph we fetched does not even contain main, so
    // we cannot tell, and answering "false" there produces a FALSE REFUSAL.
    // That bug was live: the harness reported "Hermit pin: X / Latest main: X"
    // -- identical -- while claiming X was not reachable from main.
    let main_present = git_in(graph, &["cat-file", "-e", &format!("{descendant}^{{commit}}")])?;
    if !main_present.status.success() {
        return Err(format!(
            "the fetched Reverie graph does not contain {descendant}; cannot judge \
             reachability (incomplete fetch, not a pin violation)"
        ));
    }
    let pin_present = git_in(graph, &["cat-file", "-e", &format!("{ancestor}^{{commit}}")])?;
    if !pin_present.status.success() {
        return Ok(false);
    }
    let output = git_in(graph, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git merge-base --is-ancestor {ancestor} {descendant} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// The pin recorded on the base this change would land on, for the monotonicity
/// comparison. `None` when there is no base to compare against (a fresh repo, a
/// detached probe, an unavailable ref) -- in which case monotonicity is not
/// asserted rather than being assumed satisfied.
fn base_pin(root: &Path, base_ref: &str) -> Option<String> {
    // NO `:(glob)` PATHSPEC HERE. `git ls-tree` rejects pathspec magic outright
    // -- "pathspec magic not supported by this command: 'glob'" -- so passing
    // the same spec `ls-files` accepts makes the whole call FAIL, which would
    // return None and silently skip the monotonicity assertion entirely. That
    // is a fail-OPEN hole, and it is what the regression bracket caught before
    // this shipped. List the flat tree and filter by basename instead, the same
    // way the parent's primary_checkout.py does for the same reason.
    let listed = git_in(root, &["ls-tree", "-r", "-z", "--name-only", base_ref]).ok()?;
    if !listed.status.success() {
        return None;
    }
    let mut found: BTreeSet<String> = BTreeSet::new();
    for name in String::from_utf8_lossy(&listed.stdout)
        .split('\0')
        .filter(|name| !name.is_empty() && name.ends_with("Cargo.toml"))
    {
        let blob = git_in(root, &["cat-file", "blob", &format!("{base_ref}:{name}")]).ok()?;
        if !blob.status.success() {
            continue;
        }
        for line in String::from_utf8_lossy(&blob.stdout).lines() {
            if is_reverie_git_source(line) {
                if let Some(rev) = extract_rev(line) {
                    if is_full_sha(&rev) {
                        found.insert(rev);
                    }
                }
            }
        }
    }
    // An incoherent base cannot define a floor; refuse to invent one.
    (found.len() == 1).then(|| found.into_iter().next().unwrap_or_default())
}

/// Environment acknowledgement that clears the case-3 advisory.
const ACK_ENV: &str = "HERMIT_PIN_BELOW_MASTER_ACK";

/// PRE-COMMIT ADVISORY. Exactly four cases, owner-specified 2026-08-08:
///
///   1. the commit does NOT touch pin entries          -> SILENT, exit 0.
///   2. it touches them and bumps ALL THE WAY to master -> SILENT, exit 0.
///   3. it touches them and bumps but STOPS SHORT       -> surface + require
///      acknowledgement. "Why update but leave it stale?" Deliberately touching
///      the pin and stopping short is a smell: either go to master, or say why
///      not. PROCEEDABLE, NOT BLOCKING -- pinning below a known-bad newer
///      commit, or a master that does not build yet, are legitimate.
///   4. it REGRESSES the pin                            -> SILENT here. That is
///      the CI check's monotonicity refusal, a hard refusal, and duplicating it
///      as a soft prompt would teach people to acknowledge past it.
///
/// CASE 1 IS THE LOAD-BEARING SILENCE. A commit touching zero Cargo files was
/// being refused outright today; anything printed on that path is a regression
/// of this design, so it is bracketed explicitly.
///
/// Rarity is where this gets its power: it fires only on deliberate pin edits,
/// so it stays readable instead of decaying into a reflex flag. Do not widen it.
fn staged_pin_advisory(root: &Path, remote: &str) -> Result<i32, String> {
    let staged = read_pins(root)?;
    let candidate = match unique_pin(&staged) {
        Ok(pin) => pin.to_string(),
        // Inconsistent manifests are a different, always-blocking defect that
        // the normal path reports; the advisory stays quiet rather than
        // second-guessing it.
        Err(_) => return Ok(0),
    };
    let Some(head) = base_pin(root, "HEAD") else {
        return Ok(0);
    };
    if head == candidate {
        return Ok(0); // CASE 1: no pin edit in this commit.
    }
    let main = query_main(remote)?;
    if candidate == main {
        return Ok(0); // CASE 2: bumped all the way.
    }
    let graph = reverie_graph(root, remote)?;
    if !is_ancestor(&graph, &head, &candidate)? {
        return Ok(0); // CASE 4: regression (or off-history) -- CI refuses it.
    }
    if env::var(ACK_ENV).map(|value| value == "1").unwrap_or(false) {
        return Ok(0); // CASE 3, acknowledged.
    }
    let behind = git_in(&graph, &["rev-list", "--count", &format!("{candidate}..{main}")])?;
    let lag = String::from_utf8_lossy(&behind.stdout).trim().to_string();
    loud_header("REVERIE PIN BUMPED, BUT NOT TO MASTER");
    eprintln!("Previous pin: {head}");
    eprintln!("This commit:  {candidate}");
    eprintln!("Reverie main: {main}  ({lag} commit(s) ahead of this commit's pin)");
    eprintln!();
    eprintln!("You are deliberately moving the pin but stopping short of Reverie master.");
    eprintln!("That is allowed -- pinning below a known-bad newer commit, or below a master");
    eprintln!("that does not build yet, are legitimate reasons -- but it should be a choice,");
    eprintln!("not an accident.");
    eprintln!();
    eprintln!("Go all the way:      with-proxy ./ci/run-reverie-pin-check.sh --update-to-latest");
    eprintln!("Or acknowledge:      {ACK_ENV}=1 git commit ...");
    eprintln!("  (acknowledging states you know Hermit will be on a non-master Reverie.)");
    Ok(1)
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

fn git_in(dir: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| format!("could not run git {}: {error}", args.join(" ")))
}

fn unique_pin(scan: &PinScan) -> Result<&str, String> {
    let pins: BTreeSet<&str> = scan
        .occurrences
        .iter()
        .map(|occurrence| occurrence.rev.as_str())
        .collect();
    if pins.len() != 1 {
        return Err(format!(
            "Reverie dependency metadata contains {} distinct revisions: {}",
            pins.len(),
            pins.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(scan.occurrences[0].rev.as_str())
}

fn run_cargo_update(root: &Path, args: &[&str]) -> Result<(), String> {
    let status = Command::new("cargo")
        .current_dir(root)
        .args(args)
        .status()
        .map_err(|error| format!("could not run cargo {}: {error}", args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cargo {} failed with {status}; the manifest edits remain visible for repair",
            args.join(" ")
        ))
    }
}

fn rewrite_manifest_pins(scan: &PinScan, main: &str) -> Result<(usize, usize), String> {
    let old_revisions: BTreeSet<&str> = scan
        .occurrences
        .iter()
        .map(|occurrence| occurrence.rev.as_str())
        .filter(|revision| *revision != main)
        .collect();
    if old_revisions.is_empty() {
        return Ok((0, 0));
    }

    let manifest_paths: BTreeSet<&Path> = scan
        .occurrences
        .iter()
        .filter(|occurrence| {
            occurrence
                .path
                .file_name()
                .is_some_and(|name| name == "Cargo.toml")
        })
        .map(|occurrence| occurrence.path.as_path())
        .collect();
    // ALL-OR-NOTHING. The bump spans ~20 revision entries across ~8 manifests, and a tree with
    // some advanced is worse than one with none: consumers grepping the pin see two answers, and
    // the next agent inherits a half-applied bump with no record of which half. Writing straight
    // through the loop had exactly that failure mode -- measured, by making one late manifest
    // unwritable: 18 of 20 entries advanced, 2 left stale, nothing restored. The post-condition in
    // `update_to_latest` caught the inconsistency afterwards but could not undo it, and detection
    // without restoration still leaves the tree broken.
    //
    // Compute every replacement first; write only once all are known; undo the writes already made
    // if a later one fails. Rollback can itself fail, so a partial rollback names the exact files
    // rather than being swallowed -- that is the one state a human must be told about.
    let mut planned: Vec<(&Path, String, String)> = Vec::new();
    let mut changed_entries = 0;
    for path in manifest_paths {
        let original = fs::read_to_string(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let mut updated = original.clone();
        for old in &old_revisions {
            let occurrences = updated.matches(*old).count();
            changed_entries += occurrences;
            updated = updated.replace(*old, main);
        }
        if updated != original {
            planned.push((path, original, updated));
        }
    }

    let mut written: Vec<(&Path, &str)> = Vec::new();
    for (path, original, updated) in &planned {
        match fs::write(path, updated) {
            Ok(()) => written.push((path, original.as_str())),
            Err(error) => {
                let mut unrestored = Vec::new();
                for (done_path, done_original) in &written {
                    if fs::write(done_path, done_original).is_err() {
                        unrestored.push(done_path.display().to_string());
                    }
                }
                let restored = written.len() - unrestored.len();
                let mut message = format!(
                    "could not update {}: {error}; rolled back {restored} already-written \
                     manifest(s) so the tree is not left partially bumped",
                    path.display()
                );
                if !unrestored.is_empty() {
                    message.push_str(&format!(
                        ". ROLLBACK INCOMPLETE -- still advanced, restore by hand: {}",
                        unrestored.join(", ")
                    ));
                }
                return Err(message);
            }
        }
    }
    Ok((written.len(), changed_entries))
}

/// The one site that records a JUDGEMENT rather than a reference.
///
/// This wrapper binds a *measured* build clamp and threshold to one exact
/// Reverie revision on purpose -- its own comment says the check "prevents a pin
/// bump from silently reusing an earlier revision's clamp and measured
/// threshold". Rewriting it asserts that the measurement still applies, which is
/// not something this tool can establish. Everything else that names the pin
/// outside Cargo metadata merely RESTATES this value and is carried mechanically.
const BUDGET_CALIBRATION_SITE: &str = "ci/run-with-reverie-dbt-budget.sh";

/// The revision the DBT build budget is currently calibrated for.
fn calibrated_pin(root: &Path) -> Result<Option<String>, String> {
    let path = root.join(BUDGET_CALIBRATION_SITE);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix("expected_pin=") {
            let value = value.trim().trim_matches(['"', '\''].as_slice());
            if is_full_sha(value) {
                return Ok(Some(value.to_string()));
            }
            return Err(format!(
                "{}: expected_pin= is not an exact 40-hex revision: {value:?}",
                path.display()
            ));
        }
    }
    Err(format!(
        "{}: no expected_pin= line found; the calibration site moved and this \
         tool can no longer find the decision it must not skip",
        path.display()
    ))
}

/// Carry `old` -> `main` across every tracked non-Cargo site that merely
/// RESTATES the pin, leaving [`BUDGET_CALIBRATION_SITE`] untouched.
///
/// Derived by search rather than from a hard-coded list, so a site added or
/// removed later is picked up without editing this function. Returns the files
/// touched and the number of occurrences rewritten.
fn carry_derived_pin_sites(
    root: &Path,
    old: &str,
    main: &str,
) -> Result<(Vec<PathBuf>, usize), String> {
    let output = git_in(
        root,
        &[
            "grep",
            "-l",
            "--fixed-strings",
            old,
            "--",
            ":!*Cargo.toml",
            ":!*Cargo.lock",
            &format!(":!{BUDGET_CALIBRATION_SITE}"),
        ],
    )?;
    // `git grep -l` exits 1 with no output when nothing matches; that is "no
    // derived sites", not a failure.
    if !output.status.success() && !output.stdout.is_empty() {
        return Err(format!(
            "git grep for derived pin sites failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut touched = Vec::new();
    let mut rewritten = 0;
    for relative in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
    {
        let path = root.join(relative);
        let original = fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let occurrences = original.matches(old).count();
        if occurrences == 0 {
            continue;
        }
        let updated = original.replace(old, main);
        // Report only work that actually happened: a no-op substitution must not
        // be counted as a carry, and must not rewrite the file's mtime either.
        if updated == original {
            continue;
        }
        fs::write(&path, updated)
            .map_err(|error| format!("could not update {}: {error}", path.display()))?;
        rewritten += occurrences;
        touched.push(path);
    }
    touched.sort();
    Ok((touched, rewritten))
}

/// Refuse to report success while the calibration decision is unmade.
///
/// Deliberately NOT a value this tool guesses. A bump that silently rewrote the
/// wrapper would assert a measured budget still applies, report success, and
/// look exactly like a fix -- which is worse than the hand-carry it replaced.
fn calibration_decision_required(old: &str, main: &str) -> String {
    format!(
        "\n\
         ======================================================================\n\
         REVERIE PIN: DBT BUILD-BUDGET CALIBRATION DECISION REQUIRED\n\
         ======================================================================\n\
         Cargo metadata and every derived CI site now name {main}.\n\
         {BUDGET_CALIBRATION_SITE} still names {old}, and this tool will not\n\
         change it for you: that line asserts a MEASURED build clamp and\n\
         threshold still apply, which is a judgement, not a lookup.\n\
         \n\
         Decide whether the budget carries. It governs one quantity: the elapsed\n\
         time reverie-dbt/build.rs reports for a DynamoRIO content-key miss,\n\
         hashed over {{reverie-dbt/vendor/dynamorio, reverie-dbt/build.rs,\n\
         $CMAKE, $CMAKE_GENERATOR}}. In a Reverie checkout:\n\
         \n\
         \x20 git -C <reverie> diff {old}:reverie-dbt/build.rs \\\n\
         \x20     {main}:reverie-dbt/build.rs\n\
         \x20 git -C <reverie> rev-parse {old}:reverie-dbt/vendor/dynamorio \\\n\
         \x20     {main}:reverie-dbt/vendor/dynamorio\n\
         \n\
         Changed bytes do NOT by themselves mean recalibration: judge whether the\n\
         diff can affect build TIME. A pure rename cannot. Note the DBI->DBT\n\
         rename also MOVED these paths, so a query at an older revision can\n\
         return nothing rather than a difference -- absent is not unchanged.\n\
         \n\
         If it carries: set expected_pin={main} in {BUDGET_CALIBRATION_SITE} and\n\
         append a `CARRY TO` block to ci/configure-build-jobs.sh stating the\n\
         evidence. If it does not: recalibrate and record the measurement.\n\
         Then re-run this checker; it will report the tree current.\n\
         \n\
         Nothing above needs redoing -- the Cargo sites and the derived CI sites\n\
         are already written.\n"
    )
}

fn update_to_latest(root: &Path, scan: &PinScan, main: &str) -> Result<(), String> {
    // Read the calibration BEFORE any rewrite: once the derived sites move, the
    // wrapper is the only remaining record of the revision we are carrying from.
    let calibrated = calibrated_pin(root)?;

    if scan
        .occurrences
        .iter()
        .all(|occurrence| occurrence.rev == main)
    {
        // Cargo metadata is current, but the CI sites are a separate scope and
        // may still be mid-carry -- finish them rather than reporting success
        // over a narrower scope than the caller means by "the pin".
        return finish_ci_pin_sites(root, calibrated.as_deref(), main, true);
    }

    let (changed_files, changed_entries) = rewrite_manifest_pins(scan, main)?;

    println!(
        "Updated {changed_entries} manifest revision entries in {changed_files} files; resolving tracked lockfiles."
    );
    run_cargo_update(root, &["update", "-p", "reverie-core"])?;
    let liteinst_manifest = root.join("liteinst-runtime-build/Cargo.toml");
    if liteinst_manifest.is_file() {
        let manifest = liteinst_manifest
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 manifest path: {}", liteinst_manifest.display()))?;
        run_cargo_update(
            root,
            &[
                "update",
                "--manifest-path",
                manifest,
                "-p",
                "reverie-liteinst",
            ],
        )?;
    }

    let updated = read_pins(root)?;
    let pin = unique_pin(&updated)?;
    if pin != main {
        return Err(format!(
            "update completed but derived Cargo metadata records {pin}, expected {main}"
        ));
    }
    println!(
        "Reverie pin updated to latest main {main} across {} derived Cargo revision entries.",
        updated.occurrences.len()
    );
    finish_ci_pin_sites(root, calibrated.as_deref(), main, false)
}

/// Carry the derived CI sites, then refuse to claim success if the one
/// calibration decision is still open.
///
/// Split out so the already-current path takes it too: "Cargo metadata is
/// current" is a narrower fact than "the pin is carried", and reporting the
/// former as the latter is what let 16 CI sites go stale behind a success
/// message three times in one day.
fn finish_ci_pin_sites(
    root: &Path,
    calibrated: Option<&str>,
    main: &str,
    cargo_already_current: bool,
) -> Result<(), String> {
    let Some(old) = calibrated else {
        if cargo_already_current {
            println!("Reverie pin is already current: {main}");
        }
        return Ok(());
    };
    if old == main {
        // The decision is settled and the derived sites restate this same value,
        // so there is nothing left to carry. Counting the already-correct sites
        // here would report work that did not happen.
        if cargo_already_current {
            println!("Reverie pin is already current: {main}");
        }
        return Ok(());
    }

    let (touched, rewritten) = carry_derived_pin_sites(root, old, main)?;
    println!(
        "Carried {rewritten} derived CI pin occurrence(s) from {old} in {} file(s):",
        touched.len()
    );
    for path in &touched {
        println!("  {}", path.display());
    }
    Err(calibration_decision_required(old, main))
}

fn loud_header(title: &str) {
    eprintln!("======================================================================");
    eprintln!("REVERIE PIN LINT: {title}");
    eprintln!("======================================================================");
}

fn blocked_instructions() {
    eprintln!();
    eprintln!("BLOCKED. Testing must use the latest rrnewton/reverie:main.");
    eprintln!("Update every derived manifest and lockfile site with:");
    eprintln!("  with-proxy ./ci/run-reverie-pin-check.sh --update-to-latest");
    eprintln!("Policy and recovery details: docs/updating-reverie.md");
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
/// network), and shared by all three enforcement paths (hook, scripts/validate.rs, CI)
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
    let provenance = checkout_provenance(&root);

    if config.print_pin {
        println!("{}", unique_pin(&scan)?);
        report_provenance(provenance.as_ref());
        return Ok(0);
    }

    if config.staged_advisory {
        #[cfg(not(test))]
        let advisory_remote = DEFAULT_REMOTE;
        #[cfg(test)]
        let advisory_remote = config.remote.as_deref().unwrap_or(DEFAULT_REMOTE);
        return staged_pin_advisory(&root, advisory_remote);
    }

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
    report_provenance(provenance.as_ref());

    let mut by_rev: BTreeMap<&str, Vec<&PinOccurrence>> = BTreeMap::new();
    for pin in pins {
        by_rev.entry(&pin.rev).or_default().push(pin);
    }
    if by_rev.len() != 1 && !config.update_to_latest {
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

    // Production has no CLI/env/recorded-value override for the authority.
    // Tests substitute only the remote transport, then exercise this same
    // refs/heads/main dereference rather than injecting a well-shaped SHA.
    #[cfg(not(test))]
    let remote = DEFAULT_REMOTE;
    #[cfg(test)]
    let remote = config.remote.as_deref().unwrap_or(DEFAULT_REMOTE);
    let base_ref = config.base_ref.as_str();
    let main_result = query_main(remote);

    let main = match main_result {
        Ok(main) => main,
        Err(error) => {
            loud_header("COULD NOT VERIFY LATEST REVERIE MAIN - BLOCKED");
            if let Ok(pin) = unique_pin(&scan) {
                eprintln!("Hermit pin: {pin}");
            }
            eprintln!("Lookup error: {error}");
            blocked_instructions();
            return Ok(1);
        }
    };

    if config.update_to_latest {
        update_to_latest(&root, &scan, &main)?;
        let updated = read_pins(&root)?;
        let updated_pin = unique_pin(&updated)?;
        let cache_code = check_liteinst_cache_keys(&root, updated_pin)?;
        if cache_code != 0 {
            return Ok(cache_code);
        }
        return Ok(0);
    }

    let pin = unique_pin(&scan)?;
    let cache_code = check_liteinst_cache_keys(&root, pin)?;
    if cache_code != 0 {
        return Ok(cache_code);
    }

    let entries = pins.len();
    let pin_files = pinned_file_count.len();

    // OFFLINE STOPS HERE, having decided everything that does not need the
    // network: the manifests agree with each other (checked above via
    // unique_pin) and the LiteInst cache keys track the pin. Those are real,
    // offline-decidable defects that no amount of waiting fixes, so they stay
    // BLOCKING for every caller. What offline deliberately does NOT judge is
    // currency -- see the pre-commit hook for why that must not block.
    if config.offline {
        println!(
            "Reverie pin is locally consistent: {pin} ({entries} revision entries across \
             {pin_files} tracked Cargo metadata files; currency not evaluated, --offline)"
        );
        return Ok(0);
    }

    // OWNER-APPROVED RULE (2026-08-08): ANCESTRY + MONOTONICITY, not equality.
    //
    // Equality made the comparand a LIVE MOVING REF, so the verdict was a
    // property of the tree AND THE INSTANT YOU LOOKED: two runs over a
    // byte-identical tree disagreed with nothing changed locally, and the pin
    // went stale whenever anyone pushed to Reverie (~16.6 commits/day). A pin
    // that must equal the tip is not a pin.
    //
    // ANCESTRY ALONE IS NOT ENOUGH, and this is the hole the owner caught: an
    // ANCIENT commit is also an ancestor, so ancestry by itself would happily
    // accept a pin walked BACKWARDS. Hence the second clause.
    //
    // CONFLICTS TAKE THE NEWER PIN is enforced HERE rather than by a separate
    // mechanism: resolving a Cargo.toml/Cargo.lock conflict to the older side
    // regresses the pin below the base, which MONOTONIC refuses. That is the
    // whole point of pairing them -- conflict resolution is precisely where a
    // silent regression would otherwise land unnoticed.
    if !is_full_sha(&main) {
        return Err(format!("refusing to judge against invalid main {main:?}"));
    }
    let graph = reverie_graph(&root, remote)?;

    // (1) ANCESTRY: the pin must be on Reverie's main history. This refuses a
    // dead, abandoned, or rewritten commit -- the case a tip-equality check
    // never even asked about.
    if !is_ancestor(&graph, pin, &main)? {
        loud_header("REVERIE PIN IS NOT ON reverie/main HISTORY - BLOCKED");
        eprintln!("Hermit pin:  {pin}");
        eprintln!("Latest main: {main}");
        eprintln!(
            "The pin is not reachable from rrnewton/reverie:main. It names a commit that was\n\
             abandoned, rewritten, or never merged -- so nothing on main contains it and no\n\
             amount of waiting will make it current."
        );
        eprintln!(
            "Affected metadata: {entries} revision entries across {pin_files} tracked Cargo files."
        );
        blocked_instructions();
        return Ok(1);
    }

    // (2) MONOTONIC: the pin may not regress below the base this change lands
    // on. Equal is fine (the overwhelmingly common no-op case); forward is the
    // point; backward is refused.
    // ENSURE THE BASE EXISTS BEFORE REFUSING FOR ITS ABSENCE.
    //
    // The refusal below is correct -- an unevaluated monotonicity check is
    // indistinguishable from a passing one -- but a bare actions/checkout@v4 is
    // depth 1 with no origin/main, so it fired in the `preflight` job and turned
    // main RED. Fetching it here fixes every caller at once; patching each
    // workflow job that happens to run this node is whack-a-mole.
    //
    // ONLY WHEN ABSENT, and this scoping is the point: a shared checkout is used
    // concurrently by many agents, and ADVANCING an existing origin/main under
    // them is exactly the moving-reference hazard removed elsewhere today. If
    // the ref resolves we do not touch it.
    if base_ref == DEFAULT_BASE_REF
        && !git_in(&root, &["rev-parse", "--verify", "--quiet", base_ref])
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        let _ = git_in(
            &root,
            &["fetch", "--no-tags", "--quiet", "origin", "main:refs/remotes/origin/main"],
        );
    }
    let resolved_base = base_pin(&root, base_ref);
    if resolved_base.is_none() && !config.no_base {
        return Err(format!(
            "cannot resolve a monotonicity base from {base_ref:?}: the ref is missing (a \
             depth-1 clone has no origin/main), unreadable, or pins more than one Reverie \
             revision. REFUSING rather than skipping: an unevaluated monotonicity check is \
             indistinguishable from a passing one. Fetch the base ref, pass --base-ref, or \
             pass --no-base to declare that this invocation genuinely has no base."
        ));
    }
    if let Some(base) = resolved_base {
        if base != pin && !is_ancestor(&graph, &base, pin)? {
            let direction = if is_ancestor(&graph, pin, &base)? {
                "REGRESSES to an older commit"
            } else {
                "moves sideways onto a commit that does not contain"
            };
            loud_header("REVERIE PIN REGRESSION - BLOCKED");
            eprintln!("Base ({base_ref}) pin: {base}");
            eprintln!("This change's pin:    {pin}");
            eprintln!("The pin {direction} the base pin.");
            eprintln!(
                "The pin may only advance. If this came from resolving a Cargo.toml or\n\
                 Cargo.lock conflict, RESOLVE TO THE NEWER SIDE -- taking the older side is\n\
                 exactly the silent regression this refusal exists to catch."
            );
            blocked_instructions();
            return Ok(1);
        }
    }

    let behind = git_in(&graph, &["rev-list", "--count", &format!("{pin}..{main}")])?;
    let lag = String::from_utf8_lossy(&behind.stdout).trim().to_string();
    if pin == main {
        println!(
            "Reverie pin is current: {pin} ({entries} revision entries across {pin_files} tracked Cargo metadata files)"
        );
    } else {
        println!(
            "Reverie pin is on main history and does not regress: {pin} ({lag} commit(s) behind \
             {main}; {entries} revision entries across {pin_files} tracked Cargo metadata files)"
        );
    }
    Ok(0)
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
    fn validates_full_sha() {
        assert!(is_full_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_sha("01234567"));
        assert!(!is_full_sha("z123456789abcdef0123456789abcdef01234567"));
    }

    /// A tree where the calibration site names `old` and one derived site does
    /// too, so a carry has something real to move and something real to refuse.
    fn calibration_fixture(label: &str, old: &str) -> PathBuf {
        let root = temp_path(label);
        fs::create_dir_all(root.join("ci")).expect("mkdir ci");
        fs::write(
            root.join(BUDGET_CALIBRATION_SITE),
            format!("#!/bin/bash\nexpected_pin={old}\n"),
        )
        .expect("write wrapper");
        fs::write(
            root.join("ci/configure-build-jobs.sh"),
            format!("# bound to {old}\ncheck {old}\n"),
        )
        .expect("write derived");
        init_fixture_repo(&root);
        git_in(&root, &["add", "-A"]).expect("stage fixture");
        git_in(&root, &["commit", "-q", "-m", "fixture"]).expect("commit fixture");
        root
    }

    /// NEGATIVE. The one judgement must never be defaulted.
    ///
    /// Automating the 15 derived sites while silently guessing the 16th would be
    /// worse than the hand-carry it replaces, because the tool's own success
    /// would be what hides it. So: carry the derived sites, leave the
    /// calibration exactly as found, and refuse.
    #[test]
    fn refuses_to_guess_the_budget_calibration_and_leaves_it_untouched() {
        let old = "1".repeat(40);
        let main = "2".repeat(40);
        let root = calibration_fixture("carry-refuse", &old);

        let refusal = finish_ci_pin_sites(&root, Some(&old), &main, false)
            .expect_err("an unsettled calibration must refuse, not succeed");
        assert!(refusal.contains("CALIBRATION DECISION REQUIRED"), "{refusal}");
        // Actionable, not merely negative: it must name the file to edit and the
        // value to write, or the operator is back to rediscovering the step.
        assert!(refusal.contains(BUDGET_CALIBRATION_SITE), "{refusal}");
        assert!(refusal.contains(&format!("expected_pin={main}")), "{refusal}");

        let wrapper = fs::read_to_string(root.join(BUDGET_CALIBRATION_SITE)).expect("read wrapper");
        assert!(
            wrapper.contains(&old) && !wrapper.contains(&main),
            "the calibration was rewritten instead of being left as the decision: {wrapper}"
        );
        let derived = fs::read_to_string(root.join("ci/configure-build-jobs.sh")).expect("derived");
        assert!(
            derived.contains(&main) && !derived.contains(&old),
            "the derived site should have been carried: {derived}"
        );
    }

    /// POSITIVE. Once the decision is settled the tool completes, and reports no
    /// work it did not do -- an earlier draft counted already-correct sites and
    /// claimed to have carried them.
    #[test]
    fn settled_calibration_completes_without_reporting_phantom_carries() {
        let main = "2".repeat(40);
        let root = calibration_fixture("carry-settled", &main);

        finish_ci_pin_sites(&root, Some(&main), &main, true)
            .expect("a settled calibration must complete");

        let (touched, rewritten) =
            carry_derived_pin_sites(&root, &main, &main).expect("no-op carry");
        assert_eq!(rewritten, 0, "a no-op substitution must not count as a carry");
        assert!(touched.is_empty(), "no file should be rewritten: {touched:?}");
    }

    /// The calibration site is the tool's anchor for what it must not decide.
    /// If it moves and we silently read "no pin" as "nothing to settle", the
    /// refusal disappears and the tool starts succeeding over a missing check --
    /// absence reading as agreement, which is the defect this tool exists to fix.
    #[test]
    fn a_calibration_site_without_the_marker_is_an_error_not_an_absence() {
        let root = temp_path("carry-marker");
        fs::create_dir_all(root.join("ci")).expect("mkdir ci");
        fs::write(
            root.join(BUDGET_CALIBRATION_SITE),
            "#!/bin/bash\n# the expected_pin line was moved or renamed\n",
        )
        .expect("write wrapper");

        let error = calibrated_pin(&root).expect_err("a marker-less calibration site must error");
        assert!(error.contains("no expected_pin="), "{error}");
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

    fn temp_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        env::temp_dir().join(format!(
            "check-reverie-pin-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn init_fixture_repo(root: &Path) {
        fs::create_dir_all(root).expect("create fixture repository");
        assert!(git_in(root, &["init", "-q"]).unwrap().status.success());
        assert!(
            git_in(root, &["config", "user.email", "pin-test@example.com"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(root, &["config", "user.name", "Reverie Pin Test"])
                .unwrap()
                .status
                .success()
        );
    }

    fn commit_file(root: &Path, name: &str, body: &str) -> String {
        fs::write(root.join(name), body).expect("write fixture file");
        assert!(git_in(root, &["add", name]).unwrap().status.success());
        assert!(
            git_in(root, &["commit", "-qm", name])
                .unwrap()
                .status
                .success()
        );
        String::from_utf8_lossy(&git_in(root, &["rev-parse", "HEAD"]).unwrap().stdout)
            .trim()
            .to_string()
    }

    fn set_origin_main(root: &Path, sha: &str) {
        assert!(
            git_in(root, &["update-ref", "refs/remotes/origin/main", sha])
                .unwrap()
                .status
                .success()
        );
    }

    fn checkout_detached(root: &Path, sha: &str) {
        assert!(
            git_in(root, &["checkout", "-q", "--detach", sha])
                .unwrap()
                .status
                .success()
        );
    }

    /// NEGATIVE SIDE: plant a checkout that is strictly behind `origin/main`
    /// and confirm the provenance check catches it. This is the shape that
    /// makes a correct pin read as a stale pin.
    #[test]
    fn provenance_flags_a_checkout_behind_main() {
        let root = temp_path("behind-main");
        init_fixture_repo(&root);
        let old = commit_file(&root, "a", "old\n");
        let main = commit_file(&root, "b", "new\n");
        set_origin_main(&root, &main);
        checkout_detached(&root, &old);

        let provenance = checkout_provenance(&root).expect("resolve provenance");
        assert_eq!(provenance.head, old);
        assert_eq!(
            provenance.behind_main.as_deref(),
            Some(main.as_str()),
            "a strict ancestor of origin/main must be reported as behind"
        );
    }

    /// POSITIVE SIDE: a checkout sitting exactly on `origin/main` must not
    /// warn, or the warning is noise everyone learns to ignore.
    #[test]
    fn provenance_is_silent_at_main() {
        let root = temp_path("at-main");
        init_fixture_repo(&root);
        commit_file(&root, "a", "old\n");
        let main = commit_file(&root, "b", "new\n");
        set_origin_main(&root, &main);
        checkout_detached(&root, &main);

        let provenance = checkout_provenance(&root).expect("resolve provenance");
        assert_eq!(provenance.head, main);
        assert_eq!(provenance.behind_main, None);
    }

    /// A PR head legitimately differs from `main` while carrying its own
    /// commits. Keying on inequality instead of strict ancestry would warn on
    /// every CI run and destroy the signal.
    #[test]
    fn provenance_is_silent_on_a_divergent_pr_head() {
        let root = temp_path("pr-head");
        init_fixture_repo(&root);
        let base = commit_file(&root, "a", "old\n");
        let main = commit_file(&root, "b", "new\n");
        set_origin_main(&root, &main);
        checkout_detached(&root, &base);
        let pr_head = commit_file(&root, "c", "feature\n");

        assert_ne!(pr_head, main);
        let provenance = checkout_provenance(&root).expect("resolve provenance");
        assert_eq!(provenance.head, pr_head);
        assert_eq!(
            provenance.behind_main, None,
            "a divergent PR head is not 'behind main' and must not warn"
        );
    }

    /// No `origin/main` ref (fresh clone of a fork, or a bare fixture) is a
    /// missing authority, not a violation: report HEAD, claim nothing else.
    #[test]
    fn provenance_without_origin_main_reports_head_only() {
        let root = temp_path("no-origin-main");
        init_fixture_repo(&root);
        let head = commit_file(&root, "a", "only\n");

        let provenance = checkout_provenance(&root).expect("resolve provenance");
        assert_eq!(provenance.head, head);
        assert_eq!(provenance.behind_main, None);
    }

    #[test]
    fn exact_latest_pin_passes() {
        let root = temp_path("current");
        let remote = temp_path("current-reverie");
        init_fixture_repo(&root);
        init_fixture_repo(&remote);
        fs::write(remote.join("revision"), "current\n").expect("write Reverie fixture");
        assert!(
            git_in(&remote, &["add", "revision"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["commit", "-qm", "current"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );
        let current =
            String::from_utf8_lossy(&git_in(&remote, &["rev-parse", "HEAD"]).unwrap().stdout)
                .trim()
                .to_string();
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{current}\" }}\n"
            ),
        )
        .expect("write fixture manifest");
        assert!(
            git_in(&root, &["add", "Cargo.toml"])
                .unwrap()
                .status
                .success()
        );
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(remote.to_string_lossy().into_owned()),
            // Isolated fixture: there is genuinely no base ref here, so the
            // skip is DECLARED rather than stumbled into.
            no_base: true,
            ..Config::default()
        })
        .expect("current pin should be classified");
        assert_eq!(code, 0, "an exact latest-main pin must pass");
        fs::remove_dir_all(root).expect("remove fixture repository");
        fs::remove_dir_all(remote).expect("remove Reverie fixture repository");
    }

    #[test]
    fn lagging_pin_on_main_history_passes() {
        // BRACKET 1 of 4. This assertion is DELIBERATELY INVERTED from what it
        // was: it previously required a behind-but-valid pin to fail closed,
        // which is precisely the equality rule the owner replaced. Deliberately
        // lagging an upstream is what a pin IS.
        let root = temp_path("behind-hermit");
        let remote = temp_path("behind-reverie");
        init_fixture_repo(&root);
        init_fixture_repo(&remote);

        fs::write(remote.join("revision"), "old\n").expect("write old Reverie fixture");
        assert!(
            git_in(&remote, &["add", "revision"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["commit", "-qm", "old"])
                .unwrap()
                .status
                .success()
        );
        let old = String::from_utf8_lossy(&git_in(&remote, &["rev-parse", "HEAD"]).unwrap().stdout)
            .trim()
            .to_string();
        fs::write(remote.join("revision"), "latest\n").expect("write latest Reverie fixture");
        assert!(
            git_in(&remote, &["add", "revision"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["commit", "-qm", "latest"])
                .unwrap()
                .status
                .success()
        );
        let latest =
            String::from_utf8_lossy(&git_in(&remote, &["rev-parse", "HEAD"]).unwrap().stdout)
                .trim()
                .to_string();
        assert_ne!(old, latest);
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );

        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{old}\" }}\n"
            ),
        )
        .expect("write stale Hermit fixture");
        assert!(
            git_in(&root, &["add", "Cargo.toml"])
                .unwrap()
                .status
                .success()
        );
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(remote.to_string_lossy().into_owned()),
            // Isolated fixture: there is genuinely no base ref here, so the
            // skip is DECLARED rather than stumbled into.
            no_base: true,
            ..Config::default()
        })
        .expect("behind pin should be classified");
        assert_eq!(
            code, 0,
            "a pin that is an ANCESTOR of main and does not regress must PASS: lagging is the \
             normal, intended state under ancestry+monotonicity"
        );

        fs::remove_dir_all(root).expect("remove Hermit fixture repository");
        fs::remove_dir_all(remote).expect("remove Reverie fixture repository");
    }

    /// ONE shared Reverie fixture per test process, built once.
    ///
    /// It is READ-ONLY after construction, so every test that needs a Reverie
    /// history can share it. Building a fresh 3-commit repo per test cost ~15
    /// git subprocesses each; with 4 test binaries pinned to ONE CPU in the
    /// harness's concurrency bracket that fork pressure made the suite
    /// intermittently flaky. Measured: it is the TEST work, not compilation --
    /// rustc peak RSS only moved 170.7 MB -> 178.2 MB (+4.4%) and 0.29s ->
    /// 0.35s, so 4 concurrent compiles stay far under the node's 1 GiB cap.
    /// Deliberately not removed: a shared fixture that one test deletes while
    /// another reads it is exactly the race this replaces.
    fn shared_reverie() -> &'static (PathBuf, String, String, String) {
        static SHARED: std::sync::OnceLock<(PathBuf, String, String, String)> =
            std::sync::OnceLock::new();
        SHARED.get_or_init(|| reverie_history_fixture("shared"))
    }

    /// Build a Reverie fixture with `main` = old -> latest, plus a commit on an
    /// abandoned side branch that `main` never contains. Returns
    /// (remote, old, latest, offhistory).
    fn reverie_history_fixture(label: &str) -> (PathBuf, String, String, String) {
        let remote = temp_path(label);
        init_fixture_repo(&remote);
        let head = |dir: &Path| {
            String::from_utf8_lossy(&git_in(dir, &["rev-parse", "HEAD"]).unwrap().stdout)
                .trim()
                .to_string()
        };
        let commit = |dir: &Path, body: &str, msg: &str| {
            fs::write(dir.join("revision"), body).expect("write Reverie fixture");
            assert!(git_in(dir, &["add", "revision"]).unwrap().status.success());
            assert!(git_in(dir, &["commit", "-qm", msg]).unwrap().status.success());
        };
        commit(&remote, "old\n", "old");
        let old = head(&remote);
        commit(&remote, "latest\n", "latest");
        let latest = head(&remote);
        assert!(git_in(&remote, &["branch", "-M", "main"]).unwrap().status.success());
        // An abandoned branch off `old`: reachable as an object, NOT reachable
        // from main. This is the shape of a rebased-away or never-merged commit.
        assert!(git_in(&remote, &["checkout", "-q", "-b", "abandoned", &old]).unwrap().status.success());
        commit(&remote, "abandoned\n", "abandoned");
        let offhistory = head(&remote);
        assert!(git_in(&remote, &["checkout", "-q", "main"]).unwrap().status.success());
        (remote, old, latest, offhistory)
    }

    /// Write a Hermit fixture pinning `pin`, commit it, and record `base_pin`
    /// on a `base` ref so monotonicity has a floor to compare against.
    fn hermit_fixture(label: &str, base_pin: &str, pin: &str) -> PathBuf {
        let root = temp_path(label);
        init_fixture_repo(&root);
        let manifest = |rev: &str| {
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{rev}\" }}\n"
            )
        };
        fs::write(root.join("Cargo.toml"), manifest(base_pin)).expect("write base manifest");
        assert!(git_in(&root, &["add", "Cargo.toml"]).unwrap().status.success());
        assert!(git_in(&root, &["commit", "-qm", "base"]).unwrap().status.success());
        assert!(git_in(&root, &["branch", "-f", "basefixture"]).unwrap().status.success());
        fs::write(root.join("Cargo.toml"), manifest(pin)).expect("write candidate manifest");
        assert!(git_in(&root, &["add", "Cargo.toml"]).unwrap().status.success());
        root
    }

    #[test]
    fn an_unresolvable_base_is_an_error_not_a_pass() {
        // THE THIRD STATE. Before this was specified, a repo with no resolvable
        // base ref -- exactly a depth-1 CI checkout, which is what the
        // reverie-pin job actually had -- returned rc=0 and printed "does not
        // regress". An unevaluated monotonicity check is indistinguishable from
        // a passing one, so COULD-NOT-DETERMINE must fail closed.
        let (remote, old, _latest, _off) = shared_reverie();
        let root = temp_path("nobase-hermit");
        init_fixture_repo(&root);
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{old}\" }}\n"
            ),
        )
        .expect("write manifest");
        assert!(git_in(&root, &["add", "Cargo.toml"]).unwrap().status.success());

        let undeclared = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(remote.to_string_lossy().into_owned()),
            ..Config::default() // base_ref defaults to origin/main, which does not exist here
        });
        assert!(
            undeclared.is_err(),
            "an unresolvable monotonicity base must be a CHECKER ERROR, not a pass: {undeclared:?}"
        );

        let declared = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(remote.to_string_lossy().into_owned()),
            no_base: true,
            ..Config::default()
        })
        .expect("a DECLARED absence of base is allowed");
        assert_eq!(declared, 0, "--no-base is an intended skip and must pass");

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn regressed_pin_is_refused() {
        // BRACKET 3 of 4, AND THE ONE THAT CLOSES THE HOLE. Ancestry alone would
        // ACCEPT this: `old` is a perfectly good ancestor of main. Only
        // monotonicity catches a pin walked BACKWARDS -- which is exactly what a
        // Cargo.lock conflict resolved to the older side produces.
        let (remote, old, latest, _off) = shared_reverie();
        let root = hermit_fixture("regress-hermit", &latest, &old);
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(remote.to_string_lossy().into_owned()),
            base_ref: "basefixture".to_string(),
            ..Config::default()
        })
        .expect("regressed pin should be classified");
        assert_eq!(code, 1, "a pin that REGRESSES below its base must be REFUSED");
        fs::remove_dir_all(root).expect("remove Hermit fixture repository");
    }

    #[test]
    fn pin_not_on_main_history_is_refused() {
        // BRACKET 4 of 4. The pin names a real, fetchable object that main does
        // NOT contain. Note this cannot be checked by object PRESENCE: a fetch
        // of main alone still lands such objects, so presence would wrongly
        // accept. Only reachability refuses it.
        let (remote, old, _latest, offhistory) = shared_reverie();
        let root = hermit_fixture("offhist-hermit", &old, &offhistory);
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(remote.to_string_lossy().into_owned()),
            base_ref: "basefixture".to_string(),
            ..Config::default()
        })
        .expect("off-history pin should be classified");
        assert_eq!(
            code, 1,
            "a pin not reachable from reverie/main must be REFUSED even though it is a real commit"
        );
        fs::remove_dir_all(root).expect("remove Hermit fixture repository");
    }

    /// Drive the pre-commit advisory: HEAD pins `head_pin`, the worktree stages
    /// `staged_pin`. Returns (exit code, stderr-was-produced).
    fn advisory(label: &str, remote: &Path, head_pin: &str, staged_pin: &str) -> i32 {
        let root = temp_path(label);
        init_fixture_repo(&root);
        let manifest = |rev: &str| {
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{rev}\" }}\n"
            )
        };
        fs::write(root.join("Cargo.toml"), manifest(head_pin)).expect("write HEAD manifest");
        assert!(git_in(&root, &["add", "Cargo.toml"]).unwrap().status.success());
        assert!(git_in(&root, &["commit", "-qm", "head"]).unwrap().status.success());
        if staged_pin != head_pin {
            fs::write(root.join("Cargo.toml"), manifest(staged_pin)).expect("stage manifest");
            assert!(git_in(&root, &["add", "Cargo.toml"]).unwrap().status.success());
        }
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(remote.to_string_lossy().into_owned()),
            staged_advisory: true,
            ..Config::default()
        })
        .expect("advisory should classify");
        fs::remove_dir_all(root).expect("remove fixture");
        code
    }

    #[test]
    fn advisory_case1_no_pin_touch_is_silent() {
        // THE LOAD-BEARING SILENCE. A commit that does not touch pin entries
        // must produce NOTHING -- this is the case that was refusing a
        // CI-config change touching zero Cargo files.
        let (remote, old, _latest, _off) = reverie_history_fixture("adv1-reverie");
        assert_eq!(advisory("adv1-hermit", &remote, &old, &old), 0);
    }

    #[test]
    fn advisory_case2_bump_all_the_way_is_silent() {
        let (remote, old, latest, _off) = shared_reverie();
        assert_eq!(advisory("adv2-hermit", &remote, &old, &latest), 0);
    }

    #[test]
    fn advisory_case3_bump_short_of_master_asks_for_acknowledgement() {
        // Needs a 3-commit history so a bump can land strictly between.
        let remote = temp_path("adv3-reverie");
        init_fixture_repo(&remote);
        let head = |d: &Path| {
            String::from_utf8_lossy(&git_in(d, &["rev-parse", "HEAD"]).unwrap().stdout)
                .trim()
                .to_string()
        };
        for (body, msg) in [("a\n", "a"), ("b\n", "b"), ("c\n", "c")] {
            fs::write(remote.join("revision"), body).expect("write");
            assert!(git_in(&remote, &["add", "revision"]).unwrap().status.success());
            assert!(git_in(&remote, &["commit", "-qm", msg]).unwrap().status.success());
        }
        assert!(git_in(&remote, &["branch", "-M", "main"]).unwrap().status.success());
        let tip = head(&remote);
        let first = String::from_utf8_lossy(
            &git_in(&remote, &["rev-parse", "main~2"]).unwrap().stdout,
        )
        .trim()
        .to_string();
        let middle = String::from_utf8_lossy(
            &git_in(&remote, &["rev-parse", "main~1"]).unwrap().stdout,
        )
        .trim()
        .to_string();
        assert_ne!(middle, tip);
        assert_eq!(
            advisory("adv3-hermit", &remote, &first, &middle),
            1,
            "a forward bump that stops short of master must ASK for acknowledgement"
        );
    }

    #[test]
    fn advisory_case4_regression_is_silent_here() {
        // Case 4 belongs to CI's monotonicity refusal. Prompting for a soft
        // acknowledgement here would train people to acknowledge past a hard
        // refusal, so this surface stays quiet.
        let (remote, old, latest, _off) = shared_reverie();
        assert_eq!(
            advisory("adv4-hermit", &remote, &latest, &old),
            0,
            "a regression must be SILENT on the advisory surface -- CI refuses it"
        );
    }

    #[test]
    fn forward_advance_from_a_base_passes() {
        // The monotonic-forward case, so bracket 3 cannot pass vacuously by
        // refusing every base comparison.
        let (remote, old, latest, _off) = shared_reverie();
        let root = hermit_fixture("advance-hermit", &old, &latest);
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(remote.to_string_lossy().into_owned()),
            base_ref: "basefixture".to_string(),
            ..Config::default()
        })
        .expect("forward advance should be classified");
        assert_eq!(code, 0, "advancing the pin forward must PASS");
        fs::remove_dir_all(root).expect("remove Hermit fixture repository");
    }

    #[test]
    fn mechanical_update_rewrites_derived_manifest_sites() {
        let root = temp_path("update");
        init_fixture_repo(&root);
        let old = "0123456789abcdef0123456789abcdef01234567";
        let latest = "89abcdef0123456789abcdef0123456789abcdef";
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{old}\" }}\n"
            ),
        )
        .expect("write stale fixture manifest");
        assert!(
            git_in(&root, &["add", "Cargo.toml"])
                .unwrap()
                .status
                .success()
        );
        let scan = read_pins(&root).expect("scan fixture manifest");
        assert_eq!(rewrite_manifest_pins(&scan, latest).unwrap(), (1, 1));
        let updated = read_pins(&root).expect("rescan updated fixture manifest");
        assert_eq!(unique_pin(&updated).unwrap(), latest);
        fs::remove_dir_all(root).expect("remove fixture repository");
    }

    #[test]
    fn tracked_stale_lockfile_fails_the_checker_path() {
        let root = temp_path("stale-lock");
        let remote = temp_path("stale-lock-reverie");
        let runtime = root.join("runtime");
        init_fixture_repo(&root);
        init_fixture_repo(&remote);
        fs::create_dir_all(&runtime).expect("create fixture directories");
        fs::write(remote.join("revision"), "current\n").expect("write Reverie fixture");
        assert!(
            git_in(&remote, &["add", "revision"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["commit", "-qm", "current"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );
        let current =
            String::from_utf8_lossy(&git_in(&remote, &["rev-parse", "HEAD"]).unwrap().stdout)
                .trim()
                .to_string();
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
            remote: Some(remote.to_string_lossy().into_owned()),
            // Isolated fixture: there is genuinely no base ref here, so the
            // skip is DECLARED rather than stumbled into.
            no_base: true,
            ..Config::default()
        })
        .expect("checker should classify the planted inconsistency");
        assert_eq!(code, 1, "a tracked stale Cargo.lock must fail closed");

        fs::remove_dir_all(root).expect("remove fixture repository");
        fs::remove_dir_all(remote).expect("remove Reverie fixture repository");
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
