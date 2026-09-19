#!/usr/bin/env -S rust-script --force
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
//!                    and tip equality is allowed but not required. A lagging
//!                    ancestor is legitimate; requiring the tip made the
//!                    verdict a property of WHEN you looked rather than of
//!                    the tree.
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
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

const DEFAULT_REMOTE: &str = "https://github.com/rrnewton/reverie.git";
const MAIN_REF: &str = "refs/heads/main";
const DEFAULT_BASE_REF: &str = "origin/main";
struct Config {
    repo: Option<PathBuf>,
    #[cfg(test)]
    remote: Option<String>,
    print_pin: bool,
    update_to_latest: bool,
    /// Skip the post-bump compile check. Store the unsafe choice inverted so
    /// `Config::default()` structurally keeps verification enabled.
    skip_verify_build: bool,
    /// Skip every NETWORKED judgement (ancestry, monotonicity, and the
    /// main-tip query) and decide only what is decidable offline: that the
    /// tracked manifests agree with each other, and that the LiteInst cache
    /// keys track the pin. Used by the pre-commit hook, which the owner has
    /// ruled must not be a hard blocker on distance from the main tip.
    offline: bool,
    /// Pre-commit advisory. Judges the STAGED pin against HEAD's and against
    /// Reverie main, and speaks in exactly one of four cases (see
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
            skip_verify_build: false,
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
       --update-to-latest                  Advance every derived Cargo pin site to the main tip\n\
       --no-verify-build                   Skip the post-bump compile check (UNSAFE)\n\
       --base-ref REF                      Monotonicity floor (default: origin/main)\n\
       --offline                           Local consistency only; no networked policy checks\n\
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
            "--no-verify-build" => config.skip_verify_build = true,
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
    if config.skip_verify_build && !config.update_to_latest {
        return Err("--no-verify-build requires --update-to-latest".to_string());
    }
    Ok(config)
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Discover the product checkout from the CURRENT DIRECTORY, never from an
/// inherited `GIT_DIR`.
///
/// This runs isolated for the same reason the graph queries do. An inherited
/// `GIT_DIR` wins over directory discovery, so a hook-invoked run would report
/// the INVOKING repository as the Hermit root and then check that repository's
/// pins. That is not hypothetical here: the same class of inheritance once
/// redirected a fetch and left a Hermit checkout sitting at Reverie's HEAD with
/// 3,615 apparently-dirty paths.
fn git_root() -> Result<PathBuf, String> {
    let output = under_git_env(|| {
        isolated_git_command()?
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|error| format!("could not run git rev-parse: {error}"))
    })?;
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
        let contents = fs::read_to_string(path)
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
/// Resolve the cache path to the object that will actually be written, and
/// refuse anything that is reached through a symbolic link.
///
/// ORDER IS THE WHOLE POINT HERE. The previous shape tested
/// `cache.join("HEAD").is_file()` -- which FOLLOWS links -- and then ran
/// `git init --bare <cache>` before any validation. A symlink at `cache`, or at
/// any component of the path beneath the checkout, means `git init` creates a
/// repository at the link's TARGET while the later guard inspects whatever it
/// finds afterwards. The check and the operation then apply to different
/// objects, which is the same defect class this whole file exists to close.
///
/// So: resolve first, refuse links, and only then create or open. A symlinked
/// graph cache has no legitimate use -- the path is a build artifact under
/// `target/` that this script owns -- so refusing outright is both sound and
/// simpler than reasoning about where the link points.
fn resolve_cache_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    // Canonicalize the deepest EXISTING ancestor, then re-append the components
    // below it. Canonicalizing the whole path is not possible before creation,
    // and canonicalizing nothing would leave every intermediate link unchecked.
    let mut resolved = fs::canonicalize(root).map_err(|error| {
        format!(
            "could not canonicalize the Hermit checkout {}: {error}",
            root.display()
        )
    })?;
    for component in Path::new(relative).components() {
        let name = match component {
            std::path::Component::Normal(name) => name,
            other => {
                return Err(format!(
                    "refusing a graph cache path with a non-literal component {other:?}"
                ));
            }
        };
        let candidate = resolved.join(name);
        // `symlink_metadata` does NOT follow the final component, which is what
        // makes this a link check rather than a target check.
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "REFUSING Reverie graph cache: {} is a symbolic link. The cache is a build \
                     artifact this script creates under target/; a link there would let `git \
                     init` write to the link's target while validation inspected something else.",
                    candidate.display()
                ));
            }
            Ok(_) => {
                resolved = fs::canonicalize(&candidate).map_err(|error| {
                    format!(
                        "could not canonicalize graph cache component {}: {error}",
                        candidate.display()
                    )
                })?;
            }
            // Does not exist yet: nothing to follow, and nothing below it can
            // exist either, so the remaining components are appended literally.
            Err(_) => resolved = candidate,
        }
    }
    Ok(resolved)
}

/// What the resolved cache path already is, decided by reading the FILESYSTEM
/// and nothing else.
///
/// This runs BEFORE `git init` and before any Git command is aimed at the path,
/// which is the entire point. Asking Git to classify the path would mean Git
/// performing repository discovery on attacker-shaped contents -- following a
/// `.git` FILE to a git-dir somewhere else, or walking UP out of an unrelated
/// directory into the product repository -- and the answer would then describe
/// something other than the thing about to be written.
#[derive(Debug)]
enum CacheState {
    /// Nothing there. Safe to create.
    Absent,
    /// An existing directory shaped like the bare cache this script creates.
    /// Still subject to every Git-level guard in [`GuardedGitRepo::open`].
    BareCache,
}

/// Classify the resolved cache path, refusing anything that is not clearly
/// absent or clearly our own bare cache.
///
/// The refusals here are not defence in depth behind the Git-level guards --
/// they are EARLIER THAN THE ONLY MOMENT AT WHICH THOSE GUARDS COULD RUN. The
/// previous shape ran `git init --bare <cache>` whenever `<cache>/HEAD` was not
/// a regular file, so a directory that was not a repository got initialised
/// first and inspected second. Two carriers make that dangerous:
///
/// * A `.git` FILE (`gitdir: /elsewhere`) turns the directory into a pointer.
///   `git init --bare` on it, and every later query, resolve THROUGH the
///   pointer, so the fetch writes into whatever repository it names while the
///   path this script reports is the innocent one.
/// * A nonempty directory that is not a repository at all is not ours. Running
///   `git init` inside someone else's data is a write we were never asked to
///   make, and afterwards the path looks exactly like a cache we created.
///
/// An EMPTY directory is accepted: it carries nothing, and `git init --bare`
/// into it is what a first run does anyway.
fn classify_cache_path(cache: &Path) -> Result<CacheState, String> {
    let metadata = match fs::symlink_metadata(cache) {
        Ok(metadata) => metadata,
        // Nothing to inspect. `resolve_cache_path` has already refused any
        // symlink on the way here, so a missing path is genuinely missing.
        Err(_) => return Ok(CacheState::Absent),
    };
    if metadata.file_type().is_symlink() {
        // Unreachable via `resolve_cache_path`, which refuses links; kept so
        // this function is safe to call on any path.
        return Err(format!(
            "REFUSING Reverie graph cache: {} is a symbolic link",
            cache.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "REFUSING Reverie graph cache: {} exists and is not a directory. The cache is a bare \
             repository this script creates under target/; anything else at that path belongs to \
             someone else.",
            cache.display()
        ));
    }

    // A `.git` entry of ANY kind disqualifies the path, and the file form is
    // the dangerous one: it redirects every subsequent Git command to another
    // git-dir. Checked without following the final component.
    let dot_git = cache.join(".git");
    if let Ok(dot_git_metadata) = fs::symlink_metadata(&dot_git) {
        let carrier = if dot_git_metadata.is_dir() {
            "a .git directory, so it is a non-bare checkout"
        } else {
            "a .git file, which redirects Git to a git-dir somewhere else"
        };
        return Err(format!(
            "REFUSING Reverie graph cache: {} contains {carrier}. The cache must be a bare \
             repository at exactly this path, so that the object written and the path validated \
             are the same thing.",
            cache.display()
        ));
    }

    let mut entries = fs::read_dir(cache)
        .map_err(|error| {
            format!(
                "could not read the graph cache {}: {error}",
                cache.display()
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            format!(
                "could not read the graph cache {}: {error}",
                cache.display()
            )
        })?;
    if entries.is_empty() {
        // Empty: nothing to destroy, and this is what a half-finished first run
        // leaves behind.
        return Ok(CacheState::Absent);
    }

    // Recognise our own bare cache by the three things `git init --bare`
    // always creates. `HEAD` must be a regular FILE: a symlinked HEAD is
    // another way to point the repository elsewhere.
    let head_is_file = fs::symlink_metadata(cache.join("HEAD"))
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false);
    let objects_is_dir = fs::symlink_metadata(cache.join("objects"))
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false);
    let refs_is_dir = fs::symlink_metadata(cache.join("refs"))
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false);
    if head_is_file && objects_is_dir && refs_is_dir {
        return Ok(CacheState::BareCache);
    }

    entries.sort_by_key(|entry| entry.file_name());
    let listing: Vec<String> = entries
        .iter()
        .take(8)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    Err(format!(
        "REFUSING Reverie graph cache: {} is not empty and is not a bare repository (found {}{}). \
         Refusing before `git init`, because initialising into unrecognized contents both writes \
         where we were not asked to and makes the result indistinguishable from a cache this \
         script created. Remove the path if it is stale.",
        cache.display(),
        listing.join(", "),
        if entries.len() > listing.len() {
            format!(", and {} more", entries.len() - listing.len())
        } else {
            String::new()
        }
    ))
}

fn reverie_graph(root: &Path, remote: &str) -> Result<GuardedGitRepo, String> {
    // Resolved BEFORE anything is created, so `git init` and the guard below
    // both act on the same object.
    let cache = resolve_cache_path(root, "target/ci/reverie-graph.git")?;
    // Classified from the filesystem BEFORE any Git command is aimed at the
    // path, so a `.git`-file carrier or foreign contents can never be
    // initialised into and judged afterwards.
    match classify_cache_path(&cache)? {
        CacheState::BareCache => {
            // Existing cache: validate it as a repository before reusing it.
            GuardedGitRepo::open(&cache, root)?;
        }
        CacheState::Absent => {
            fs::create_dir_all(cache.parent().unwrap_or(&cache))
                .map_err(|error| format!("could not create the Reverie graph cache: {error}"))?;
            let init = under_git_env(|| {
                isolated_git_command()?
                    .args(["init", "--bare", "--quiet"])
                    .arg(&cache)
                    .output()
                    .map_err(|error| format!("could not run git init: {error}"))
            })?;
            if !init.status.success() {
                return Err(format!(
                    "git init --bare failed: {}",
                    String::from_utf8_lossy(&init.stderr).trim()
                ));
            }
        }
    }
    let graph = GuardedGitRepo::open(&cache, root)?;
    // The fetch below contacts `remote`, and what it brings back is the graph
    // every ancestry answer is read from. Same authority, same check.
    refuse_rewritten_authority_url(remote)?;
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
    let fetch = graph.run(&[
        "fetch",
        "--no-tags",
        "--quiet",
        "--force",
        remote,
        "+refs/heads/main:refs/heads/main",
    ])?;
    if !fetch.status.success() {
        return Err(format!(
            "could not fetch the Reverie commit graph from {remote}: {}",
            String::from_utf8_lossy(&fetch.stderr).trim()
        ));
    }
    Ok(graph)
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
fn is_ancestor(graph: &GuardedGitRepo, ancestor: &str, descendant: &str) -> Result<bool, String> {
    // ABSENT ANCESTOR is a genuine verdict: the graph has main, and the pin is
    // not in it, so the pin is not reachable. ABSENT DESCENDANT is NOT a
    // verdict -- it means the graph we fetched does not even contain main, so
    // we cannot tell, and answering "false" there produces a FALSE REFUSAL.
    // That bug was live: the harness reported "Hermit pin: X / Reverie main: X"
    // -- identical -- while claiming X was not reachable from main.
    let main_present = graph.run(&["cat-file", "-e", &format!("{descendant}^{{commit}}")])?;
    if !main_present.status.success() {
        return Err(format!(
            "the fetched Reverie graph does not contain {descendant}; cannot judge \
             reachability (incomplete fetch, not a pin violation)"
        ));
    }
    let pin_present = graph.run(&["cat-file", "-e", &format!("{ancestor}^{{commit}}")])?;
    if !pin_present.status.success() {
        return Ok(false);
    }
    let output = graph.run(&["merge-base", "--is-ancestor", ancestor, descendant])?;
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
    let listed = authority_git_in(root, &["ls-tree", "-r", "-z", "--name-only", base_ref]).ok()?;
    if !listed.status.success() {
        return None;
    }
    let mut found: BTreeSet<String> = BTreeSet::new();
    for name in String::from_utf8_lossy(&listed.stdout)
        .split('\0')
        .filter(|name| !name.is_empty() && name.ends_with("Cargo.toml"))
    {
        let blob =
            authority_git_in(root, &["cat-file", "blob", &format!("{base_ref}:{name}")]).ok()?;
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
///   2. it touches them and advances to the main tip    -> SILENT, exit 0.
///   3. it touches them and bumps but STOPS SHORT       -> surface + require
///      acknowledgement. "Why advance without selecting the tip?" Deliberately
///      touching the pin and stopping short deserves an explicit choice.
///      PROCEEDABLE, POLICY-COMPLIANT, NOT BLOCKING -- pinning below a
///      known-bad newer commit, or a main tip that does not build yet, are
///      legitimate.
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
    let behind = graph.run(&["rev-list", "--count", &format!("{candidate}..{main}")])?;
    let lag = String::from_utf8_lossy(&behind.stdout).trim().to_string();
    loud_header("REVERIE PIN ADVANCED BELOW THE MAIN TIP - ACKNOWLEDGEMENT");
    eprintln!("Previous pin: {head}");
    eprintln!("This commit:  {candidate}");
    eprintln!("Reverie main: {main}  ({lag} commit(s) ahead of this commit's pin)");
    eprintln!();
    eprintln!("You are deliberately moving the pin but not selecting the Reverie main tip.");
    eprintln!("That is policy-compliant: the pin is on main history and moves forward.");
    eprintln!("Pinning below a known-bad newer commit, or below a main tip that does not");
    eprintln!("build yet, are legitimate reasons; this advisory only asks that it be a choice.");
    eprintln!();
    eprintln!("Go all the way:      with-proxy ./ci/run-reverie-pin-check.sh --update-to-latest");
    eprintln!("Or acknowledge:      {ACK_ENV}=1 git commit ...");
    eprintln!("  (The environment variable keeps its historical name for compatibility.)");
    Ok(1)
}

fn query_main(remote: &str) -> Result<String, String> {
    // The tip this returns IS the authority: every later comparison is against
    // it. Prove the URL is the one named before asking it anything.
    refuse_rewritten_authority_url(remote)?;
    let output = under_git_env(|| {
        authority_git_command()?
            .args(["ls-remote", "--exit-code", remote, MAIN_REF])
            .output()
            .map_err(|error| format!("could not run git ls-remote: {error}"))
    })?;
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

/// Git deliberately exports repository-local variables to hooks. `git -C`
/// does not override them: an inherited `GIT_DIR` still selects that repository
/// after changing directory. Any command aimed at a different repository must
/// therefore clear Git's complete, version-specific local environment first.
fn git_local_env_vars() -> Result<&'static [OsString], String> {
    static LOCAL_ENV_VARS: OnceLock<Result<Vec<OsString>, String>> = OnceLock::new();
    let vars = LOCAL_ENV_VARS.get_or_init(|| {
        // ⚠️ THIS FORK RUNS UNDERNEATH `under_git_env` -- `clear_git_local_env`
        // calls it -- so it must NOT take the lock. That would be a recursive
        // read acquisition on one thread. It is made immune instead:
        // `rev-parse --local-env-vars` needs no configuration whatsoever, so the
        // numbered-override spelling is stripped from the child outright and it
        // cannot observe a half-published `GIT_CONFIG_COUNT` with no matching
        // `GIT_CONFIG_KEY_0`. (`KEY_n`/`VALUE_n` without a `COUNT` are ignored by
        // Git, so removing the count is what closes it.)
        //
        // This site matters more than the others: the result is memoised in a
        // `OnceLock`, and so is the `Err`. A single transient failure here would
        // be cloned back to every later caller for the lifetime of the process,
        // and since `clear_git_local_env` depends on it, EVERY isolated Git
        // command in the run would fail from then on -- including the ones the
        // lock protects.
        let output = Command::new("git")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .args(["rev-parse", "--local-env-vars"])
            .output()
            .map_err(|error| format!("could not enumerate Git local environment variables: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git rev-parse --local-env-vars failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let vars: Vec<OsString> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|name| !name.is_empty())
            .map(OsString::from)
            .collect();
        for required in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            if !vars.iter().any(|name| name == required) {
                return Err(format!(
                    "git rev-parse --local-env-vars omitted required variable {required}; refusing to run an isolated Git command"
                ));
            }
        }
        Ok(vars)
    });
    match vars {
        Ok(vars) => Ok(vars),
        Err(error) => Err(error.clone()),
    }
}

/// `git rev-parse --local-env-vars` reports two DIFFERENT kinds of variable and
/// they must not be treated alike.
///
/// Most of them SELECT A REPOSITORY -- `GIT_DIR`, `GIT_WORK_TREE`,
/// `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY` and friends -- and those are exactly
/// what has to be cleared before aiming a subprocess at another repository.
///
/// But the list also contains the caller's CONFIGURATION, which selects nothing
/// and is not ours to discard. `GIT_CONFIG_COUNT` and `GIT_CONFIG_PARAMETERS`
/// are the environment spellings of `git -c`. On this fleet the forward-proxy
/// wrapper sets exactly those: `GIT_CONFIG_COUNT=3` with three
/// `url.https://github.com/.insteadOf` rewrites in `GIT_CONFIG_KEY_n` /
/// `GIT_CONFIG_VALUE_n`. Clearing `GIT_CONFIG_COUNT` does not merely drop one
/// variable -- it ORPHANS the whole numbered set, because Git reads the count
/// first and never looks at the keys. The fetch then bypasses the proxy and
/// fails, or worse, silently reaches a different URL than the caller asked for.
///
/// Note that `GIT_CONFIG_KEY_n` and `GIT_CONFIG_VALUE_n` are NOT in Git's local
/// list, so they are never removed. That asymmetry is what makes the bug quiet:
/// the keys survive, the count does not, and the overrides vanish with nothing
/// to show for it.
const CONFIG_OVERRIDE_ENV_VARS: [&str; 2] = ["GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"];

/// Clear Git's repository-selecting environment, and PRESERVE the caller's
/// configuration overrides across that clearing.
///
/// The preserved values are re-applied explicitly rather than merely skipped.
/// Skipping would work today, since `Command` inherits the parent environment,
/// but it would stop working the moment any caller adds `env_clear()`, and the
/// intent would not be visible at the call site.
fn clear_git_local_env(command: &mut Command) -> Result<(), String> {
    let preserved: Vec<(&str, OsString)> = CONFIG_OVERRIDE_ENV_VARS
        .iter()
        .filter_map(|name| env::var_os(name).map(|value| (*name, value)))
        .collect();
    for name in git_local_env_vars()? {
        command.env_remove(name);
    }
    for (name, value) in preserved {
        command.env(name, value);
    }
    Ok(())
}

fn isolated_git_command() -> Result<Command, String> {
    let mut command = Command::new("git");
    clear_git_local_env(&mut command)?;
    Ok(command)
}

/// A Git command whose OUTPUT DECIDES THE VERDICT: the remote tip, the presence
/// of a commit, and the ancestry relation between the pin and main. Every such
/// command is built here, and here only.
///
/// Isolation alone is not enough for these. Two mechanisms can change the
/// ANSWER without changing the repository or the arguments, and both are
/// reachable from the ambient environment this script is launched in:
///
/// * REPLACEMENT REFS. `refs/replace/<oid>` substitutes one object for another
///   at read time, and `git replace --graft` exists precisely to re-parent a
///   commit. Measured on git 2.53: with a replacement in place,
///   `merge-base --is-ancestor <off-history> <main>` reports 0 -- an
///   off-history pin certified as on-history -- and reports 1 again under
///   `--no-replace-objects`. That is the whole verdict of this checker,
///   inverted by a ref an attacker can write into the cache.
/// * GRAFTS. The older `info/grafts` mechanism also rewrites parentage.
///   A real opposing control demonstrated that `core.graftFile=/dev/null`
///   does not disable it on the installed Git. Use Git's supported per-child
///   `GIT_GRAFT_FILE` override; `--no-replace-objects` alone does not cover it.
///
/// `GIT_NO_REPLACE_OBJECTS` is set as well as the flag passed. The flag governs
/// this process; the variable governs any Git the command re-enters, and the
/// two together mean no descendant reads a replacement either.
fn authority_git_command() -> Result<Command, String> {
    let mut command = isolated_git_command()?;
    // Both spellings, for the reason in the doc comment above.
    command.arg("--no-replace-objects");
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    // Set after environment isolation so the effective override cannot be
    // cleared or inherited from a caller. No repository/global config changes.
    command.env("GIT_GRAFT_FILE", "/dev/null");
    Ok(command)
}

/// Base manifest bytes decide monotonicity just as graph ancestry does. Keep
/// setup/mutation commands on git_in, but read the recorded base immutably.
fn authority_git_in(dir: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    under_git_env(|| {
        authority_git_command()?
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map_err(|error| format!("could not read immutable Git base: {error}"))
    })
}

/// Every `url.<base>.insteadOf` rewrite visible in the environment, as
/// (pattern, replacement-base) pairs.
///
/// Only the numbered `GIT_CONFIG_COUNT` form is parsed. It is unambiguous:
/// `GIT_CONFIG_KEY_n` is one whole key and `GIT_CONFIG_VALUE_n` one whole
/// value, with no quoting to interpret. `GIT_CONFIG_PARAMETERS` is handled
/// separately and conservatively for exactly the opposite reason.
fn env_url_rewrites() -> Vec<(String, String)> {
    let mut rewrites = Vec::new();
    let count = env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    for index in 0..count {
        let (Ok(key), Ok(value)) = (
            env::var(format!("GIT_CONFIG_KEY_{index}")),
            env::var(format!("GIT_CONFIG_VALUE_{index}")),
        ) else {
            continue;
        };
        // Git config keys are case-insensitive in the section and the final
        // component, and case-SENSITIVE in the subsection between them -- the
        // subsection here is the replacement URL, so it is preserved verbatim.
        let lowered = key.to_ascii_lowercase();
        let Some(rest) = lowered.strip_prefix("url.") else {
            continue;
        };
        for suffix in [".insteadof", ".pushinsteadof"] {
            if rest.ends_with(suffix) {
                let base_len = "url.".len() + rest.len() - suffix.len();
                rewrites.push((value.clone(), key[..base_len]["url.".len()..].to_string()));
                break;
            }
        }
    }
    rewrites
}

/// Apply Git's `insteadOf` rule to `remote` and return the URL Git would
/// actually contact, or `None` when no rewrite applies.
///
/// Git picks the LONGEST matching pattern, so the same rule is applied here
/// rather than the first match.
fn rewritten_remote(remote: &str, rewrites: &[(String, String)]) -> Option<String> {
    let best = rewrites
        .iter()
        .filter(|(pattern, _)| !pattern.is_empty() && remote.starts_with(pattern.as_str()))
        .max_by_key(|(pattern, _)| pattern.len())?;
    let (pattern, replacement) = best;
    let rewritten = format!("{replacement}{}", &remote[pattern.len()..]);
    (rewritten != remote).then_some(rewritten)
}

/// Refuse to ask an authority question of a URL the caller did not name.
///
/// `GIT_CONFIG_COUNT` and `GIT_CONFIG_PARAMETERS` are preserved through
/// isolation because they carry the caller's configuration -- on this fleet the
/// forward-proxy wrapper's rewrites -- and dropping them breaks the fetch. But
/// the same variables can carry `url.<attacker>.insteadOf =
/// https://github.com/rrnewton/reverie.git`, and then `ls-remote` answers with
/// the attacker's tip while every message in this script still names the real
/// remote. Measured: that redirection works, silently, and the checker then
/// validates the pin against a repository the attacker controls.
///
/// So the rewrites are not dropped and not trusted: a rewrite that does not
/// touch the authority URL is left alone, and one that CHANGES it is refused,
/// loudly, naming both URLs. Refusing rather than stripping keeps the proxy
/// working and makes the redirection impossible to mistake for a network error.
fn refuse_rewritten_authority_url(remote: &str) -> Result<(), String> {
    if let Some(rewritten) = rewritten_remote(remote, &env_url_rewrites()) {
        return Err(format!(
            "REFUSING to resolve the Reverie authority through a rewritten URL: an inherited \
             url.<base>.insteadOf override redirects {remote} to {rewritten}. The pin verdict is \
             computed from that remote, so a rewrite of it decides the verdict. Unset \
             GIT_CONFIG_COUNT / GIT_CONFIG_KEY_n / GIT_CONFIG_VALUE_n, or point them somewhere \
             that does not rewrite this URL."
        ));
    }
    // The quoted `GIT_CONFIG_PARAMETERS` spelling cannot be split back into
    // keys and values without reimplementing Git's quoting, and guessing wrong
    // here would mean either a false refusal or a missed redirection. It is not
    // set on this fleet, so a conservative refusal costs nothing and cannot be
    // wrong in the dangerous direction.
    if let Ok(parameters) = env::var("GIT_CONFIG_PARAMETERS") {
        if parameters.to_ascii_lowercase().contains("insteadof") {
            return Err(format!(
                "REFUSING to resolve the Reverie authority: GIT_CONFIG_PARAMETERS carries a URL \
                 rewrite ({parameters:?}) and its quoting cannot be parsed reliably enough to \
                 prove the rewrite does not redirect {remote}. Use the numbered \
                 GIT_CONFIG_COUNT form, which this checker can inspect."
            ));
        }
    }
    Ok(())
}

fn isolated_git_in(dir: &Path, args: &[&str]) -> Result<Output, String> {
    // The twin of `git_in`, and it needs the same guard for the same reason.
    under_git_env(|| {
        isolated_git_command()?
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map_err(|error| format!("could not run isolated git {}: {error}", args.join(" ")))
    })
}

fn canonical_git_path(repo: &Path, selector: &str) -> Result<PathBuf, String> {
    let output = isolated_git_in(repo, &["rev-parse", selector])?;
    if !output.status.success() {
        return Err(format!(
            "git -C {} rev-parse {selector} failed: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let reported = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let path = if reported.is_absolute() {
        reported
    } else {
        repo.join(reported)
    };
    fs::canonicalize(&path).map_err(|error| {
        format!(
            "could not canonicalize Git path {} reported for {}: {error}",
            path.display(),
            repo.display()
        )
    })
}

/// A bare repository proven distinct from the product repository. Keeping the
/// validated git-dir in this type makes every later graph query use the same
/// isolated command path instead of relying on each caller to remember it.
#[derive(Debug)]
struct GuardedGitRepo {
    git_dir: PathBuf,
}

impl GuardedGitRepo {
    /// Refuse any cache that shares a COMMON git-dir with the product repository.
    ///
    /// Comparing the cache's `--git-dir` against the product's `--git-common-dir`
    /// is not enough, and the gap it leaves is the dangerous one. In a LINKED
    /// WORKTREE `--git-dir` reports the per-worktree directory
    /// `<common>/worktrees/<name>`, which never equals `<common>`, so the check
    /// passes -- while every object written through that handle lands in the
    /// PRODUCT REPOSITORY'S SHARED OBJECT STORE. A `fetch --force` into a name
    /// like `refs/heads/main` then rewrites a product ref.
    ///
    /// `--git-common-dir` is the identity that actually answers "is this the same
    /// repository": it is stable across a repository and all of its linked
    /// worktrees. Both sides are resolved that way. The `--git-dir` equality is
    /// kept as well, so a plain non-worktree alias is still named precisely in
    /// the error.
    fn open(cache: &Path, product_root: &Path) -> Result<Self, String> {
        let cache_git_dir = canonical_git_path(cache, "--git-dir")?;
        let cache_common_dir = canonical_git_path(cache, "--git-common-dir")?;
        let product_common_dir = canonical_git_path(product_root, "--git-common-dir")?;
        if cache_git_dir == product_common_dir {
            return Err(format!(
                "REFUSING Reverie graph cache: cache git-dir {} resolves to the Hermit common git-dir {}",
                cache_git_dir.display(),
                product_common_dir.display()
            ));
        }
        if cache_common_dir == product_common_dir {
            return Err(format!(
                "REFUSING Reverie graph cache: cache {} is a linked worktree of the Hermit \
                 repository (shared common git-dir {}); writing there would mutate the product \
                 object store",
                cache.display(),
                product_common_dir.display()
            ));
        }
        // A NON-BARE cache is the precondition for the incident this file
        // exists to prevent, not a style preference. A repository with a
        // worktree has a checked-out HEAD, and `fetch --force
        // +refs/heads/main:refs/heads/main` -- which this script runs -- will
        // move a checked-out branch. That is how a foreign fetch once updated a
        // checked-out `main` in this repository and left a Hermit checkout
        // sitting at Reverie's HEAD, reporting 3,615 apparently-dirty paths.
        // Nothing above proves bareness: distinctness from the product only
        // establishes that it is a DIFFERENT repository, not a safe one.
        let bare = under_git_env(|| {
            isolated_git_command()?
                .arg("--git-dir")
                .arg(&cache_git_dir)
                .args(["rev-parse", "--is-bare-repository"])
                .output()
                .map_err(|error| {
                    format!("could not query the graph cache repository kind: {error}")
                })
        })?;
        if !bare.status.success() {
            return Err(format!(
                "could not determine whether the Reverie graph cache {} is bare: {}",
                cache_git_dir.display(),
                String::from_utf8_lossy(&bare.stderr).trim()
            ));
        }
        let bare = String::from_utf8_lossy(&bare.stdout).trim().to_owned();
        if bare != "true" {
            return Err(format!(
                "REFUSING Reverie graph cache: {} is not a bare repository \
                 (rev-parse --is-bare-repository reported {bare:?}). A cache with a worktree has \
                 a checked-out HEAD that this script's `fetch --force` into refs/heads/main can \
                 move.",
                cache_git_dir.display()
            ));
        }
        // IDENTITY: the path passed in, the git-dir Git resolves, and the
        // common-dir must all be the SAME directory.
        //
        // Every one of these being equal is what makes the path this script
        // reports and the directory it writes to the same thing. Each
        // inequality is a distinct redirection that the checks above do not
        // catch, because none of them compares against the CACHE PATH:
        //
        //  * git-dir != cache -- the path is a pointer, not a repository. A
        //    `.git` file inside it names another git-dir, and every write lands
        //    there. `classify_cache_path` refuses that carrier before `git
        //    init`; this catches any other route to the same shape.
        //  * common-dir != git-dir -- the repository is a linked worktree of
        //    SOMETHING. The product case is refused above by name; this refuses
        //    the rest, where objects still land in a shared store that is not
        //    at this path.
        let cache_resolved = fs::canonicalize(cache).map_err(|error| {
            format!(
                "could not canonicalize the graph cache {}: {error}",
                cache.display()
            )
        })?;
        if cache_git_dir != cache_resolved {
            return Err(format!(
                "REFUSING Reverie graph cache: {} resolves to the git-dir {}, which is a \
                 different directory. The cache must BE the repository, so that the path this \
                 script validates and the object store it writes are the same thing.",
                cache_resolved.display(),
                cache_git_dir.display()
            ));
        }
        if cache_common_dir != cache_git_dir {
            return Err(format!(
                "REFUSING Reverie graph cache: {} is a linked worktree (git-dir {}, common-dir \
                 {}); its objects land in a store that is not at the cache path.",
                cache_resolved.display(),
                cache_git_dir.display(),
                cache_common_dir.display()
            ));
        }
        // TRULY bare, not merely `core.bare=true`. The flag above is a config
        // value, and a repository that also sets `core.worktree` has a worktree
        // regardless of what it claims -- which restores exactly the
        // checked-out-HEAD hazard the bareness check exists to exclude.
        let worktree = under_git_env(|| {
            isolated_git_command()?
                .arg("--git-dir")
                .arg(&cache_git_dir)
                .args(["config", "--get", "core.worktree"])
                .output()
                .map_err(|error| format!("could not query the graph cache worktree: {error}"))
        })?;
        let configured_worktree = String::from_utf8_lossy(&worktree.stdout).trim().to_owned();
        if !configured_worktree.is_empty() {
            return Err(format!(
                "REFUSING Reverie graph cache: {} reports itself bare but configures \
                 core.worktree={configured_worktree:?}, so it has a working tree and a \
                 checked-out HEAD that `fetch --force` can move.",
                cache_git_dir.display()
            ));
        }
        Ok(Self {
            git_dir: cache_git_dir,
        })
    }

    /// Every graph query runs through here, so every ancestry answer is
    /// computed with replacement refs and grafts disabled -- see
    /// [`authority_git_command`]. This is the method that decides the verdict.
    fn run(&self, args: &[&str]) -> Result<Output, String> {
        under_git_env(|| {
            authority_git_command()?
                .arg("--git-dir")
                .arg(&self.git_dir)
                .args(args)
                .output()
                .map_err(|error| format!("could not run isolated git {}: {error}", args.join(" ")))
        })
    }
}

/// Run Git against `dir`, with Git's repository-selecting environment cleared.
///
/// `-C` changes directory; it does NOT override an inherited `GIT_DIR`, which
/// still selects the inherited repository after the chdir. Without this, an
/// explicit `--repo PATH` is only as trustworthy as the environment the checker
/// happened to be launched from -- the user names one repository and Git reads
/// another. Every caller here aims at a specific checkout, so every caller wants
/// the isolated form.
fn git_in(dir: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    under_git_env(|| {
        isolated_git_command()?
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map_err(|error| format!("could not run git {}: {error}", args.join(" ")))
    })
}

/// Hold the read side of the environment lock across `body`.
///
/// ⚠️ `body` MUST BUILD THE COMMAND AS WELL AS FORK IT. There are two reads of
/// the ambient environment at two different instants, and a guard around only
/// one of them admits a mismatched pair:
///
/// * `clear_git_local_env` SNAPSHOTS `GIT_CONFIG_COUNT` (via
///   `CONFIG_OVERRIDE_ENV_VARS`) while building the command, and pins that value
///   onto the child.
/// * `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n` are not in that list at all. They
///   are neither preserved nor removed, so the child INHERITS them at the fork.
///
/// Build outside and fork inside and you can pin `COUNT=1` from before a
/// writer's restore and then inherit no `KEY_0` after it, handing the child a
/// count and keys from two different instants. That spelling looks correct and
/// is not; it was proposed during review of this change and caught before it
/// landed.
///
/// ⚠️ RETRACTED 2026-08-26, AND THE RETRACTION IS LEFT VISIBLE ON PURPOSE. This
/// paragraph previously read "the mismatched pair is real, but **no git
/// available here rejects one**", over a table whose first three rows said
/// `exit 0`. THAT WAS FALSE, and it was mine -- landed in `cec4602d32`.
///
/// EVERY condition in the table is REJECTED, on BOTH gits, with exit 128:
///
/// | condition | git 2.52.0 | git 2.53.0-Meta | message |
/// | --- | --- | --- | --- |
/// | `COUNT=1`, no `KEY_0` | **128** | **128** | `missing config key GIT_CONFIG_KEY_0` |
/// | `COUNT=1`, `KEY_0` set, no `VALUE_0` | **128** | **128** | `missing config value GIT_CONFIG_VALUE_0` |
/// | `COUNT=2`, only pair 0 present | **128** | **128** | `missing config key GIT_CONFIG_KEY_1` |
/// | `KEY_0` empty | 128 | 128 | `empty config key` |
/// | `COUNT` non-numeric | 128 | 128 | `bogus count` |
///
/// Re-measured 2026-08-26 with `GIT_CONFIG_COUNT` and every `KEY_n`/`VALUE_n`
/// explicitly unset before each probe, on `/usr/bin/git` (2.52.0) and
/// `/usr/local/bin/git.meta.real` (2.53.0-Meta) -- the real binary, because
/// `/usr/local/bin/git` is a wrapper.
///
/// ⚠️ WHY THE FIRST MEASUREMENT WAS WRONG: THE PROBE INHERITED WHAT IT CLAIMED
/// WAS ABSENT. The `GIT_CONFIG_*` family is inherited from the AGENT HARNESS
/// PROCESS -- `GIT_CONFIG_COUNT=3` with `KEY_0..KEY_2` all set to
/// `url.https://github.com/.insteadOf`, the proxy's GitHub URL rewrites, present
/// in the process environment before any shell starts. NOT set by shell
/// initialisation: `/etc/profile`, `/etc/profile.d`, `~/.bashrc`,
/// `~/.bash_profile` and `~/.profile` mention none of them -- grepped, zero hits.
/// A login shell therefore proves nothing either way, because `bash -l` inherits
/// from whatever launched it.
///
/// AND NO EXECUTED PROGRAM COULD HAVE SET THEM, WHATEVER THE `git` ON PATH IS.
/// A child -- ELF binary, shell script, anything -- gets a COPY of the
/// environment and cannot write back to its parent. Stated as a measurement
/// because an earlier revision of this paragraph offered "it is an ELF binary"
/// as the reason, and that is not the reason:
///
/// ```text
/// wrapper.sh:   #!/bin/sh
///               export ZZPROBE=1
///               exec /bin/true
///
/// unset ZZPROBE; ./wrapper.sh; echo "${ZZPROBE-<unset>}"   ->  <unset>
/// ```
///
/// A shell-script wrapper cannot do it either, so ELF-ness distinguishes
/// nothing and is not evidence about a wrapper -- which matters here, because
/// the paragraph above calls this same binary a wrapper. Being a CHILD is the
/// reason; being compiled is not.
///
/// Running `env GIT_CONFIG_COUNT=1 git ...` overrides only the COUNT; `KEY_0` is
/// still there, so git found a key and exited 0. Measured both ways in the same
/// shell, 20 runs each: with `KEY_0` inherited, 0/20 non-zero; with it unset,
/// 20/20 non-zero.
///
/// ⚠️ AND THE POSITIVE CONTROL PASSED, BOTH THEN AND NOW. `zzz.probe=HIT`
/// appearing in `config --list` proved the overrides were HONOURED. It never
/// tested that `KEY_0` was ABSENT, which is the precondition the whole table
/// rested on. A control adjacent to the precondition looks exactly like a
/// control, and having run one is not the same as having run the right one.
///
/// WHAT THIS CHANGES ABOUT THE ARGUMENT. The mismatched pair is still real, and
/// it is NOT silently tolerated -- it is a hard 128 with a named message. So the
/// hazard is a loud failure at the fork, not a misconfigured child that runs on
/// with the wrong settings. That is a better failure than the one this paragraph
/// used to describe, and the guard's value has to be argued on those terms
/// rather than on "git will not notice".
///
/// `with_config_override`
/// restores with `remove_var` and never writes an empty key or a non-numeric
/// count, so it cannot produce either failing condition. The suite was run 30x
/// with NO guard at all (10 at default thread count, 20 at `--test-threads=32`)
/// and failed 0 times.
///
/// ⚠️ THAT 0-OF-30 IS A REAL OBSERVATION AND THE CONCLUSION DRAWN FROM IT WAS
/// WRONG. This comment used to say the guard was "hardening against a real
/// non-atomicity, not a reproduced failure", and told the reader not to cite it
/// as a fix for observed flakiness. `agent(hermit-001)` took that invitation and
/// falsified it by mutation -- same box, same minutes, one line changed:
///
/// | variant | result |
/// | --- | --- |
/// | `under_git_env` made a no-op | **FAILED 9 of 10 runs** |
/// | head as submitted | **FAILED 0 of 10 runs**, 45/45 every time |
///
/// The failing SET varied between runs -- `a_nonempty_unrecognized_cache_is_refused`,
/// `advisory_case2_bump_all_the_way_is_silent`,
/// `advisory_case3_bump_short_of_master` -- which is a RACE SIGNATURE, not a
/// broken assertion. **THIS GUARD FIXES REPRODUCED FLAKINESS. CITE IT AS ONE,
/// AND DO NOT DELETE IT.**
///
/// ⚠️ WHY THE 0-OF-30 MISSED IT, AS FAR AS THE RECORD SUPPORTS: the mutation
/// ran at LOAD AVERAGE ~39 throughout; the 30x run recorded no load figure. A
/// race that needs contention does not appear on a quiet box, and raising
/// `--test-threads` alone did not supply it. AN ABSENCE OF FAILURES AT AN
/// UNRECORDED LOAD IS NOT EVIDENCE OF ABSENCE -- state the load, or the number
/// invites exactly the deletion this paragraph now exists to prevent.
///
/// ⚠️ DO NOT NEST. Two read acquisitions on one thread is recursive read
/// locking, which `std::sync::RwLock` does not promise is safe -- a writer
/// queued between them deadlocks a writer-preferring implementation, and
/// `RwLock::read` documents that it may panic when the lock is already held by
/// the current thread. Every guarded site is therefore a leaf with respect to
/// the others. `git_local_env_vars` runs UNDERNEATH these and deliberately takes
/// no guard; it is made config-independent instead.
fn under_git_env<T>(body: impl FnOnce() -> T) -> T {
    #[cfg(test)]
    let _env_guard = tests::env_read_guard();
    body()
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
                // RESTORE THE FAILING FILE TOO -- it is the one most likely to be
                // damaged. `fs::write` truncates before it writes, so a write that
                // fails partway (ENOSPC is the realistic case) leaves THIS manifest
                // truncated or half-written. It never entered `written`, so rolling
                // back only `written` skipped exactly that file while the error text
                // claimed the tree was no longer partially bumped.
                let mut restore: Vec<(&Path, &str)> = written.clone();
                restore.push((path, original.as_str()));

                let mut unrestored = Vec::new();
                for (done_path, done_original) in &restore {
                    if fs::write(done_path, done_original).is_err() {
                        unrestored.push(done_path.display().to_string());
                    }
                }
                let restored = restore.len() - unrestored.len();
                let mut message = format!(
                    "could not update {}: {error}; restored {restored} manifest(s), including \
                     the one whose own write failed, so the tree is not left partially bumped",
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
         Then re-run this checker; it will report the tree policy-compliant.\n\
         \n\
         Nothing above needs redoing -- the Cargo sites and the derived CI sites\n\
         are already written.\n"
    )
}

fn update_to_latest(
    root: &Path,
    scan: &PinScan,
    main: &str,
    verify_build: bool,
) -> Result<(), String> {
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
        return finish_and_verify_pin_update(root, calibrated.as_deref(), main, true, verify_build);
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
        "Reverie pin advanced to main tip {main} across {} derived Cargo revision entries.",
        updated.occurrences.len()
    );
    finish_and_verify_pin_update(root, calibrated.as_deref(), main, false, verify_build)
}

/// Finish every non-build carry before judging whether the bumped tree builds.
///
/// `finish_ci_pin_sites` can still refuse an unsettled DBT calibration. Running
/// it first preserves that decision boundary and ensures the compile result is
/// about the complete candidate tree, not a half-carried pin.
fn finish_and_verify_pin_update(
    root: &Path,
    calibrated: Option<&str>,
    main: &str,
    cargo_already_current: bool,
    verify_build: bool,
) -> Result<(), String> {
    finish_and_verify_pin_update_with(
        root,
        calibrated,
        main,
        cargo_already_current,
        verify_build,
        Path::new("cargo"),
        &[],
    )
}

fn finish_and_verify_pin_update_with(
    root: &Path,
    calibrated: Option<&str>,
    main: &str,
    cargo_already_current: bool,
    verify_build: bool,
    cargo_program: &Path,
    cargo_prefix_args: &[&str],
) -> Result<(), String> {
    finish_ci_pin_sites(root, calibrated, main, cargo_already_current)?;
    if verify_build {
        verify_bumped_tree_builds(root, cargo_program, cargo_prefix_args)
    } else {
        eprintln!(
            "WARNING: --no-verify-build was passed: pin consistency was checked, but pin \
             viability was not. The bumped tree may not compile."
        );
        Ok(())
    }
}

/// Compile the complete bumped tree without permitting lockfile re-resolution.
fn verify_bumped_tree_builds(
    root: &Path,
    cargo_program: &Path,
    cargo_prefix_args: &[&str],
) -> Result<(), String> {
    println!(
        "Verifying the bumped tree compiles (cargo check --locked --workspace --all-targets)..."
    );
    let status = Command::new(cargo_program)
        .current_dir(root)
        .args(cargo_prefix_args)
        .args(["check", "--locked", "--workspace", "--all-targets"])
        .status()
        .map_err(|error| format!("could not run cargo check for the bumped tree: {error}"))?;
    if status.success() {
        println!("Bumped tree compiles.");
        return Ok(());
    }
    Err(format!(
        "BUMP REFUSED: the pin was updated consistently, but the tree does not compile \
         against it ({status}). The edits remain on disk for inspection; do not commit them."
    ))
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
            println!("Reverie pin already equals the main tip: {main}");
        }
        return Ok(());
    };
    if old == main {
        // The decision is settled and the derived sites restate this same value,
        // so there is nothing left to carry. Counting the already-correct sites
        // here would report work that did not happen.
        if cargo_already_current {
            println!("Reverie pin already equals the main tip: {main}");
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
    eprintln!(
        "BLOCKED. The pin must be on rrnewton/reverie:main history and must not regress the landing base."
    );
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
    eprintln!(
        "LiteInst cache keys: {checked} revision-keyed token(s) all track the pin ({short})."
    );
    Ok(0)
}

/// Every 40-hex revision that appears as CODE (not prose) in one of these
/// files is a DBT budget calibration binding and must equal the canonical pin.
const DBT_BUDGET_BINDING_FILES: [&str; 2] =
    [BUDGET_CALIBRATION_SITE, "ci/configure-build-jobs.sh"];

/// Exactly-40-hex tokens on a line, ignoring longer hex runs.
///
/// The length bound is load-bearing in BOTH directions. A bare `[0-9a-f]{40}`
/// also matches the first 40 characters of the 64-hex DynamoRIO recipe key that
/// lives in the same file, which would report a permanent false violation; and
/// accepting shorter tokens would match ordinary hex in a message.
fn exact_40_hex_tokens(line: &str) -> Vec<String> {
    line.split(|c: char| !c.is_ascii_hexdigit())
        .filter(|t| t.len() == 40 && t.chars().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()))
        .map(str::to_string)
        .collect()
}

/// Bind the DBT budget calibration to the canonical Reverie pin.
///
/// ⚠️ THIS COUPLING IS INVISIBLE FROM THE BUMP SIDE, WHICH IS THE WHOLE REASON
/// THE CHECK EXISTS. `ci/run-with-reverie-dbt-budget.sh` hard-codes the one
/// Reverie revision its effective-job-seconds budget was measured against, and
/// `ci/configure-build-jobs.sh` hard-codes the same revision a second time.
/// Both DECLINE with exit 75 on any mismatch, in either direction. Nothing in
/// the bump path mentions either file, so moving the pin disables every node
/// behind that wrapper and the only way to find out is to suspect it.
///
/// MEASURED, 2026-08-26: the pin moved ad598995 -> 926d931a at 21:22 and the
/// DBT/SaBRe build node declined from then until it was recalibrated, unnoticed
/// for over an hour. The decline was NOT silent -- exit 75 is `no_result` and
/// `scripts/validate.rs` reports `FINAL_VALIDATE_STATUS: COULD_NOT_RUN`, so no
/// run claimed PASSED while those nodes had no verdict -- but the coverage was
/// really gone and no one was told at the moment it went.
///
/// RELATIONSHIP TO [`calibrated_pin`], WHICH ALREADY READS ONE OF THESE FILES.
/// That reader serves the pin-UPDATE path, which refuses to rewrite
/// [`BUDGET_CALIBRATION_SITE`] because rewriting it would assert that a
/// measurement still applies -- something this tool cannot establish. That
/// reasoning is unchanged and this check does not rewrite anything either. What
/// it adds is coverage on the ORDINARY path, which the pre-commit hook and
/// `scripts/validate.rs` both run, and coverage of
/// `ci/configure-build-jobs.sh`, which holds the same revision a second time
/// and which nothing checked at all.
///
/// SCOPE IS DELIBERATELY NARROW. Only NON-COMMENT lines in the two files above
/// are read. Those files carry a long carry-history in comments naming every
/// previous calibration revision, and `ci/liteinst-strict-node.sh` writes an
/// arbitrary 40-hex string into a `.revision` self-test fixture. Neither is a
/// binding and neither may block a bump.
fn check_dbt_budget_bindings(root: &Path, pin: &str) -> Result<i32, String> {
    let mut violations: Vec<(String, String)> = Vec::new();
    let mut absent: Vec<&str> = Vec::new();
    let mut bindingless: Vec<&str> = Vec::new();
    let mut checked = 0usize;
    for rel in DBT_BUDGET_BINDING_FILES {
        let path = root.join(rel);
        let contents = match fs::read_to_string(&path) {
            Ok(c) => c,
            // ⚠️ ABSENT IS SKIPPED, AND THE SKIP IS REPORTED RATHER THAN SILENT.
            // An earlier version made this a hard error, reasoning that a check
            // which skips its own subject is worthless. That broke six existing
            // tests, which build synthetic repositories holding only Cargo
            // metadata -- measured 45 passed/0 failed before, 42/6 after. The
            // reasoning was right and the remedy was wrong: those repositories
            // genuinely have no binding to check, and erroring turned "nothing
            // to judge" into "the tree is broken". The count below is what keeps
            // it honest -- a real Hermit checkout reports 3 bindings, so a 0
            // is visible to anyone reading the line.
            Err(_) => {
                absent.push(rel);
                continue;
            }
        };
        let mut in_this_file = 0usize;
        for (idx, line) in contents.lines().enumerate() {
            if line.trim_start().starts_with('#') {
                continue;
            }
            for sha in exact_40_hex_tokens(line) {
                checked += 1;
                in_this_file += 1;
                if sha != pin {
                    violations.push((format!("{rel}:{}", idx + 1), sha));
                }
            }
        }
        if in_this_file == 0 {
            bindingless.push(rel);
        }
    }
    // ⚠️ A PRESENT BINDING FILE THAT CONTAINS NO BINDING IS A REFUSAL, NOT A PASS.
    // Found by a broken probe of this very check: a test that truncated the file
    // instead of editing it left the gate reporting success over an EMPTY file,
    // because "no bindings" and "no violations" are the same thing to the loop
    // above. That is the shape this repository keeps paying for -- a check whose
    // subject disappeared still reports green.
    // ⚠️ PER FILE, NOT IN AGGREGATE. An earlier version asked whether ANY binding
    // was found anywhere, which let one file lose its binding entirely while the
    // other still had one -- measured: deleting the configure-build-jobs.sh
    // binding returned rc=0 and the gate said nothing. Each present file must
    // carry at least one.
    if !bindingless.is_empty() {
        loud_header("DBT BUDGET BINDINGS MISSING - BLOCKED");
        eprintln!("Canonical Reverie pin: {pin}");
        eprintln!("These files are present but contain NO 40-hex calibration binding:");
        for rel in &bindingless {
            eprintln!("  {rel}");
        }
        eprintln!("The binding this gate exists to check is gone, so the gate can no longer");
        eprintln!("see a stale calibration. Restore it, or remove this check deliberately.");
        return Ok(1);
    }
    if !violations.is_empty() {
        loud_header("DBT BUDGET CALIBRATION DRIFT - BLOCKED");
        eprintln!("Canonical Reverie pin: {pin}");
        eprintln!("These DBT budget bindings do NOT equal the pin, so every node behind");
        eprintln!("ci/run-with-reverie-dbt-budget.sh will DECLINE with exit 75 (no_result):");
        for (location, sha) in &violations {
            eprintln!("  {location}: {sha}");
        }
        eprintln!();
        eprintln!("BOTH FILES MUST MOVE TOGETHER. Updating only the wrapper leaves");
        eprintln!("configure-build-jobs.sh declining with a message naming the same condition.");
        eprintln!("Before recalibrating, confirm the budget still applies at the new pin:");
        eprintln!("  reverie-dbt/vendor/dynamorio, reverie-dbt/build.rs and third-party/ must be");
        eprintln!("  byte-identical by git object id between the pins, and a cold rebuild must");
        eprintln!("  land the same DynamoRIO install key. Record that carry in the wrapper's");
        eprintln!("  comment: the gate says the coupling broke, the comment says why it exists.");
        return Ok(1);
    }
    if absent.is_empty() {
        eprintln!(
            "DBT budget bindings: {checked} binding(s) all equal the pin ({}).",
            &pin[..7.min(pin.len())]
        );
    } else {
        eprintln!(
            "DBT budget bindings: {checked} binding(s) all equal the pin ({}); NOT PRESENT in this tree: {}.",
            &pin[..7.min(pin.len())],
            absent.join(", ")
        );
    }
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
            loud_header("COULD NOT VERIFY REVERIE MAIN HISTORY - BLOCKED");
            if let Ok(pin) = unique_pin(&scan) {
                eprintln!("Hermit pin: {pin}");
            }
            eprintln!("Lookup error: {error}");
            blocked_instructions();
            return Ok(1);
        }
    };

    if config.update_to_latest {
        update_to_latest(&root, &scan, &main, !config.skip_verify_build)?;
        let updated = read_pins(&root)?;
        let updated_pin = unique_pin(&updated)?;
        let cache_code = check_liteinst_cache_keys(&root, updated_pin)?;
        if cache_code != 0 {
            return Ok(cache_code);
        }
        let budget_code = check_dbt_budget_bindings(&root, updated_pin)?;
        if budget_code != 0 {
            return Ok(budget_code);
        }
        return Ok(0);
    }

    let pin = unique_pin(&scan)?;
    let cache_code = check_liteinst_cache_keys(&root, pin)?;
    if cache_code != 0 {
        return Ok(cache_code);
    }
    let budget_code = check_dbt_budget_bindings(&root, pin)?;
    if budget_code != 0 {
        return Ok(budget_code);
    }

    let entries = pins.len();
    let pin_files = pinned_file_count.len();

    // OFFLINE STOPS HERE, having decided everything that does not need the
    // network: the manifests agree with each other (checked above via
    // unique_pin) and the LiteInst cache keys track the pin. Those are real,
    // offline-decidable defects that no amount of waiting fixes, so they stay
    // BLOCKING for every caller. What offline deliberately does NOT judge is
    // remote-policy compliance -- see the pre-commit hook for why that must not
    // block.
    if config.offline {
        println!(
            "Reverie pin is locally consistent: {pin} ({entries} revision entries across \
             {pin_files} tracked Cargo metadata files; remote policy not evaluated, --offline)"
        );
        return Ok(0);
    }

    // OWNER-APPROVED RULE (2026-08-08): ANCESTRY + MONOTONICITY; equality is
    // allowed but not required.
    //
    // Requiring equality made the comparand a LIVE MOVING REF, so the verdict
    // was a property of the tree AND THE INSTANT YOU LOOKED: two runs over a
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
        eprintln!("Reverie main: {main}");
        eprintln!(
            "The pin is not reachable from rrnewton/reverie:main. It names a commit that was\n\
             abandoned, rewritten, or never merged -- so nothing on main contains it and no\n\
             amount of waiting will put it on main history."
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
            &[
                "fetch",
                "--no-tags",
                "--quiet",
                "origin",
                "main:refs/remotes/origin/main",
            ],
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

    let behind = graph.run(&["rev-list", "--count", &format!("{pin}..{main}")])?;
    let lag = String::from_utf8_lossy(&behind.stdout).trim().to_string();
    if pin == main {
        println!(
            "Reverie pin equals the main tip: {pin} ({entries} revision entries across {pin_files} tracked Cargo metadata files)"
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
        assert!(
            refusal.contains("CALIBRATION DECISION REQUIRED"),
            "{refusal}"
        );
        // Actionable, not merely negative: it must name the file to edit and the
        // value to write, or the operator is back to rediscovering the step.
        assert!(refusal.contains(BUDGET_CALIBRATION_SITE), "{refusal}");
        assert!(
            refusal.contains(&format!("expected_pin={main}")),
            "{refusal}"
        );

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
        assert_eq!(
            rewritten, 0,
            "a no-op substitution must not count as a carry"
        );
        assert!(
            touched.is_empty(),
            "no file should be rewritten: {touched:?}"
        );
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

    fn compile_fixture(label: &str, source: &str) -> PathBuf {
        let root = temp_path(label);
        fs::create_dir_all(root.join("src")).expect("create compile fixture");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"pin-build-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .expect("write fixture manifest");
        fs::write(
            root.join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"pin-build-fixture\"\nversion = \"0.0.0\"\n",
        )
        .expect("write fixture lockfile");
        fs::write(root.join("src/lib.rs"), source).expect("write fixture source");
        root
    }

    fn fixture_cargo(root: &Path) -> PathBuf {
        let path = root.join("fixture-cargo");
        fs::write(
            &path,
            "#!/bin/sh\nset -eu\ncase \"${1:-}\" in\n  metadata)\n    test -f Cargo.toml\n    test -f Cargo.lock\n    printf '{\"packages\":[]}\\n'\n    ;;\n  check)\n    test \"$*\" = 'check --locked --workspace --all-targets'\n    mkdir -p target\n    rustc --edition=2021 --crate-type=lib src/lib.rs --out-dir target \\\n      >target/rustc.stdout 2>target/rustc.stderr\n    ;;\n  *) exit 64 ;;\nesac\n",
        )
        .expect("write fixture cargo");
        path
    }

    #[test]
    fn post_carry_compile_gate_accepts_one_building_tree_and_refuses_one_nonbuilding_tree() {
        let main = "2".repeat(40);
        let building = compile_fixture("building-bump", "pub fn value() -> u8 { 1 }\n");
        let nonbuilding = compile_fixture(
            "nonbuilding-bump",
            "pub fn value() -> u8 { missing_after_resolvable_bump() }\n",
        );
        let building_cargo = fixture_cargo(&building);
        let nonbuilding_cargo = fixture_cargo(&nonbuilding);

        let resolved = Command::new("/bin/sh")
            .current_dir(&nonbuilding)
            .arg(&nonbuilding_cargo)
            .args(["metadata", "--locked", "--format-version", "1"])
            .output()
            .expect("run cargo metadata for nonbuilding fixture");
        assert!(
            resolved.status.success(),
            "the negative fixture must resolve successfully before its compile refusal is meaningful: {}",
            String::from_utf8_lossy(&resolved.stderr)
        );

        let mut accepted = 0;
        let mut refused = 0;
        finish_and_verify_pin_update_with(
            &building,
            None,
            &main,
            true,
            true,
            Path::new("/bin/sh"),
            &[building_cargo.to_str().expect("UTF-8 fixture cargo path")],
        )
        .expect("a complete pin carry whose tree builds must be accepted");
        accepted += 1;

        let error = finish_and_verify_pin_update_with(
            &nonbuilding,
            None,
            &main,
            true,
            true,
            Path::new("/bin/sh"),
            &[nonbuilding_cargo
                .to_str()
                .expect("UTF-8 fixture cargo path")],
        )
        .expect_err("a resolvable pin carry whose tree does not build must be refused");
        assert!(error.contains("BUMP REFUSED"), "{error}");
        assert!(error.contains("does not compile"), "{error}");
        refused += 1;

        assert_eq!((accepted, refused), (1, 1));
        fs::remove_dir_all(building).expect("remove building fixture");
        fs::remove_dir_all(nonbuilding).expect("remove nonbuilding fixture");
    }

    #[test]
    fn calibration_refusal_precedes_build_verification() {
        let old = "1".repeat(40);
        let main = "2".repeat(40);
        let root = compile_fixture(
            "finish-before-build",
            "pub fn value() -> u8 { missing_after_resolvable_bump() }\n",
        );
        fs::create_dir_all(root.join("ci")).expect("create CI fixture directory");
        fs::write(
            root.join(BUDGET_CALIBRATION_SITE),
            format!("#!/bin/bash\nexpected_pin={old}\n"),
        )
        .expect("write calibration fixture");
        fs::write(
            root.join("ci/configure-build-jobs.sh"),
            format!("# derived pin\ncheck {old}\n"),
        )
        .expect("write derived pin fixture");
        init_fixture_repo(&root);
        assert!(git_in(&root, &["add", "-A"]).unwrap().status.success());

        let missing_cargo = root.join("must-not-run-cargo");
        let error = finish_and_verify_pin_update_with(
            &root,
            Some(&old),
            &main,
            false,
            true,
            &missing_cargo,
            &[],
        )
        .expect_err("an unsettled DBT calibration must refuse before cargo check runs");
        assert!(error.contains("CALIBRATION DECISION REQUIRED"), "{error}");
        assert!(
            !error.contains("BUMP REFUSED"),
            "build verification ran before finish_ci_pin_sites: {error}"
        );
        fs::remove_dir_all(root).expect("remove ordering fixture");
    }

    #[test]
    fn build_verification_is_enabled_by_default() {
        assert!(
            !Config::default().skip_verify_build,
            "the unsafe opt-out must never be the derived default"
        );
    }

    #[test]
    fn help_states_the_checker_scope() {
        let help = usage();
        assert!(help.contains("every tracked Cargo.toml and Cargo.lock"));
        assert!(help.contains("Excludes non-Cargo files"));
        assert!(help.contains("--no-verify-build"));
        assert!(help.contains("UNSAFE"));
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

    /// Every REPOSITORY-SELECTING variable must be cleared, and a poisoned
    /// config override must never reach the child either.
    ///
    /// This test previously asserted that EVERY name Git reports as local is
    /// cleared, config overrides included. That is what erased the caller's
    /// `-c` settings -- see `CONFIG_OVERRIDE_ENV_VARS`. The assertion is not
    /// relaxed here so much as split: the selection variables are still checked
    /// exactly as before, and the config variables gain a STRICTER check than
    /// they had, because it is no longer enough for them to be absent or
    /// present -- their value must be the PARENT's, never the poison.
    #[test]
    fn isolated_commands_clear_every_git_local_environment_variable() {
        let vars = git_local_env_vars().expect("enumerate Git local environment");
        const POISON: &str = "poisoned-by-parent-git-process";
        let mut command = Command::new("env");
        for name in vars {
            command.env(name, POISON);
        }
        clear_git_local_env(&mut command).expect("clear Git local environment");
        let output = command.output().expect("print child environment");
        assert!(output.status.success());
        let child_env = String::from_utf8_lossy(&output.stdout);
        let value_of = |name: &str| -> Option<String> {
            let assignment = format!("{name}=");
            child_env
                .lines()
                .find(|line| line.starts_with(&assignment))
                .map(|line| line[assignment.len()..].to_owned())
        };

        for name in vars {
            let name = name.to_string_lossy().into_owned();
            if CONFIG_OVERRIDE_ENV_VARS.contains(&name.as_str()) {
                // Not a repository selection. It must carry the PARENT's value,
                // or be absent when the parent has none -- but never the poison.
                match (env::var(&name).ok(), value_of(&name)) {
                    (Some(parent), Some(child)) => assert_eq!(
                        child, parent,
                        "{name} must reach the child as the parent's value, not {child}"
                    ),
                    (None, child) => assert_eq!(
                        child, None,
                        "{name} is unset in the parent, so nothing may reach the child"
                    ),
                    (Some(parent), None) => {
                        panic!("{name} was set to {parent} in the parent and must be preserved")
                    }
                }
                assert_ne!(
                    value_of(&name).as_deref(),
                    Some(POISON),
                    "{name} leaked the poisoned parent-Git value into the child"
                );
                continue;
            }
            assert!(
                value_of(&name).is_none(),
                "isolated command retained {name}, which selects a repository"
            );
        }
    }

    #[test]
    fn inherited_git_dir_imports_foreign_main_but_cleared_fetch_is_safe() {
        let root = temp_path("git-env-victim");
        let remote = temp_path("git-env-foreign");
        let cache = root.join("target/ci/raw-cache.git");
        init_fixture_repo(&root);
        init_fixture_repo(&remote);

        let victim_main = commit_file(&root, "victim", "Hermit\n");
        assert!(
            git_in(&root, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );
        let foreign_main = commit_file(&remote, "foreign", "Reverie\n");
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );
        fs::create_dir_all(&cache).expect("create raw cache directory");

        // This is the exact incident shape. `-C cache` changes directory, but
        // inherited GIT_DIR still selects the product repository. core.bare
        // removes Git's checked-out-branch refusal, so the foreign main lands
        // in the product repository.
        assert!(
            git_in(&root, &["config", "core.bare", "true"])
                .unwrap()
                .status
                .success()
        );
        let victim_git_dir = root.join(".git");
        let vulnerable = under_git_env(|| {
            Command::new("git")
                .env("GIT_DIR", &victim_git_dir)
                .arg("-C")
                .arg(&cache)
                .args([
                    "fetch",
                    "--no-tags",
                    "--quiet",
                    "--force",
                    remote.to_str().expect("UTF-8 fixture path"),
                    "+refs/heads/main:refs/heads/main",
                ])
                .output()
                .expect("run vulnerable fetch")
        });
        assert!(
            vulnerable.status.success(),
            "the regression bracket must reproduce the import: {}",
            String::from_utf8_lossy(&vulnerable.stderr)
        );
        let imported = under_git_env(|| {
            Command::new("git")
                .arg("--git-dir")
                .arg(&victim_git_dir)
                .args(["rev-parse", "refs/heads/main"])
                .output()
                .expect("read imported main")
        });
        assert_eq!(
            String::from_utf8_lossy(&imported.stdout).trim(),
            foreign_main,
            "the raw command must demonstrate the foreign-ref import"
        );

        // Restore only the disposable fixture, then run the same fetch after
        // applying the production environment isolation. The foreign ref must
        // land in the cache and the product main must remain unchanged.
        assert!(under_git_env(|| Command::new("git")
            .arg("--git-dir")
            .arg(&victim_git_dir)
            .args(["update-ref", "refs/heads/main", &victim_main])
            .status()
            .expect("restore fixture main")
            .success()));
        assert!(under_git_env(|| Command::new("git")
            .arg("--git-dir")
            .arg(&victim_git_dir)
            .args(["config", "core.bare", "false"])
            .status()
            .expect("restore fixture core.bare")
            .success()));
        let init = under_git_env(|| {
            isolated_git_command()
                .expect("build isolated init")
                .args(["init", "--bare", "--quiet"])
                .arg(&cache)
                .output()
                .expect("initialize cache")
        });
        assert!(init.status.success());

        // `clear_git_local_env` is called explicitly here, so the guard has to
        // span it as well as the fork -- that snapshot is half of the race.
        let safe_fetch = under_git_env(|| {
            let mut safe = Command::new("git");
            safe.env("GIT_DIR", &victim_git_dir);
            clear_git_local_env(&mut safe).expect("isolate cache fetch");
            safe.arg("--git-dir")
                .arg(&cache)
                .args([
                    "fetch",
                    "--no-tags",
                    "--quiet",
                    "--force",
                    remote.to_str().expect("UTF-8 fixture path"),
                    "+refs/heads/main:refs/heads/main",
                ])
                .output()
                .expect("run isolated fetch")
        });
        assert!(safe_fetch.status.success());
        assert_eq!(
            String::from_utf8_lossy(
                &git_in(&root, &["rev-parse", "refs/heads/main"])
                    .unwrap()
                    .stdout
            )
            .trim(),
            victim_main,
            "isolated cache fetch must not move product main"
        );
        assert_eq!(
            String::from_utf8_lossy(
                &isolated_git_in(&cache, &["rev-parse", "refs/heads/main"])
                    .unwrap()
                    .stdout
            )
            .trim(),
            foreign_main,
            "isolated fetch must still update the intended cache"
        );

        fs::remove_dir_all(root).expect("remove victim fixture");
        fs::remove_dir_all(remote).expect("remove foreign fixture");
    }

    #[test]
    fn reverie_graph_refuses_cache_that_resolves_to_product_common_git_dir() {
        use std::os::unix::fs::symlink;

        let root = temp_path("same-gitdir-victim");
        let remote = temp_path("same-gitdir-foreign");
        init_fixture_repo(&root);
        init_fixture_repo(&remote);
        let victim_main = commit_file(&root, "victim", "Hermit\n");
        assert!(
            git_in(&root, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );
        commit_file(&remote, "foreign", "Reverie\n");
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );

        let cache = root.join("target/ci/reverie-graph.git");
        fs::create_dir_all(cache.parent().expect("cache parent")).expect("create cache parent");
        symlink(root.join(".git"), &cache).expect("point cache at product git-dir");

        let error = reverie_graph(&root, remote.to_str().expect("UTF-8 fixture path"))
            .expect_err("cache resolving to product common git-dir must be refused");
        // This fixture builds the alias with a SYMLINK, so the symlink guard is
        // the one that fires, and it fires EARLIER than the identity guard did
        // -- before `git init` runs at all, which is the point of resolving
        // before acting. The safety assertion below is unchanged, and the
        // identity guard keeps its own coverage in
        // `reverie_graph_refuses_a_real_directory_aliasing_the_product` and in
        // `guarded_repo_refuses_a_linked_worktree_of_the_product`.
        assert!(
            error.contains("REFUSING Reverie graph cache")
                && (error.contains("is a symbolic link")
                    || error.contains("resolves to the Hermit common git-dir")),
            "refusal must identify a violated cache guard: {error}"
        );
        assert_eq!(
            String::from_utf8_lossy(
                &git_in(&root, &["rev-parse", "refs/heads/main"])
                    .unwrap()
                    .stdout
            )
            .trim(),
            victim_main,
            "the refusal must fire before any foreign ref update"
        );

        fs::remove_dir_all(root).expect("remove victim fixture");
        fs::remove_dir_all(remote).expect("remove foreign fixture");
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
            assert!(
                git_in(dir, &["commit", "-qm", msg])
                    .unwrap()
                    .status
                    .success()
            );
        };
        commit(&remote, "old\n", "old");
        let old = head(&remote);
        commit(&remote, "latest\n", "latest");
        let latest = head(&remote);
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );
        // An abandoned branch off `old`: reachable as an object, NOT reachable
        // from main. This is the shape of a rebased-away or never-merged commit.
        assert!(
            git_in(&remote, &["checkout", "-q", "-b", "abandoned", &old])
                .unwrap()
                .status
                .success()
        );
        commit(&remote, "abandoned\n", "abandoned");
        let offhistory = head(&remote);
        assert!(
            git_in(&remote, &["checkout", "-q", "main"])
                .unwrap()
                .status
                .success()
        );
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
        assert!(
            git_in(&root, &["add", "Cargo.toml"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&root, &["commit", "-qm", "base"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&root, &["branch", "-f", "basefixture"])
                .unwrap()
                .status
                .success()
        );
        fs::write(root.join("Cargo.toml"), manifest(pin)).expect("write candidate manifest");
        assert!(
            git_in(&root, &["add", "Cargo.toml"])
                .unwrap()
                .status
                .success()
        );
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
        assert!(
            git_in(&root, &["add", "Cargo.toml"])
                .unwrap()
                .status
                .success()
        );

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
        assert_eq!(
            code, 1,
            "a pin that REGRESSES below its base must be REFUSED"
        );
        fs::remove_dir_all(root).expect("remove Hermit fixture repository");
    }

    #[test]
    fn immutable_admission_base_survives_real_remote_advance_without_weakening_landing_floor() {
        let (reverie, old, latest, _off) = shared_reverie();
        let origin = hermit_fixture("admitted-origin", &old, &old);
        let head = || {
            String::from_utf8(git_in(&origin, &["rev-parse", "HEAD"]).unwrap().stdout)
                .unwrap()
                .trim()
                .to_string()
        };
        let floor = head();
        let checkout = temp_path("admitted-checkout");
        let cloned = Command::new("git")
            .args(["clone", "--quiet"])
            .arg(&origin)
            .arg(&checkout)
            .output()
            .unwrap();
        assert!(
            cloned.status.success(),
            "{}",
            String::from_utf8_lossy(&cloned.stderr)
        );
        let run = |base: &str| {
            run_with_config(Config {
                repo: Some(checkout.clone()),
                remote: Some(reverie.to_string_lossy().into_owned()),
                base_ref: base.into(),
                ..Config::default()
            })
            .expect("real checker result")
        };
        assert_eq!(run(&floor), 0, "early admitted invocation");
        fs::write(origin.join("Cargo.toml"), format!(
            "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{latest}\" }}\n"
        )).unwrap();
        assert!(
            git_in(&origin, &["add", "Cargo.toml"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&origin, &["commit", "-qm", "advance main pin"])
                .unwrap()
                .status
                .success()
        );
        let advanced = head();
        assert_ne!(advanced, floor);
        assert!(
            git_in(
                &checkout,
                &[
                    "fetch",
                    "--quiet",
                    "origin",
                    "HEAD:refs/remotes/origin/main"
                ]
            )
            .unwrap()
            .status
            .success()
        );
        assert_eq!(run(&floor), 0, "late invocation of the same admitted floor");
        assert_eq!(
            run("origin/main"),
            1,
            "ordinary landing still checks newer main and refuses regression"
        );
        assert_eq!(
            run(&advanced),
            1,
            "a literal newer floor also refuses the genuinely older pin"
        );
        fs::remove_dir_all(checkout).unwrap();
        fs::remove_dir_all(origin).unwrap();
    }

    #[test]
    fn admission_immutable_base_ignores_replacement_tree() {
        let (_remote, old, latest, _off) = shared_reverie();
        let root = hermit_fixture("admission-base-replacement", &latest, &old);
        assert!(
            git_in(&root, &["commit", "-qm", "candidate tree"])
                .unwrap()
                .status
                .success()
        );
        let floor = String::from_utf8(git_in(&root, &["rev-parse", "basefixture"]).unwrap().stdout)
            .unwrap();
        let floor = floor.trim();
        assert_eq!(base_pin(&root, floor).as_deref(), Some(latest.as_str()));
        let tree = |rev: &str| {
            String::from_utf8(
                git_in(&root, &["rev-parse", &format!("{rev}^{{tree}}")])
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim()
            .to_string()
        };
        let floor_tree = tree(floor);
        let current_tree = tree("HEAD");
        assert_ne!(floor_tree, current_tree);
        assert!(
            git_in(&root, &["replace", &floor_tree, &current_tree])
                .unwrap()
                .status
                .success()
        );
        let raw = git_in(&root, &["show", &format!("{floor}:Cargo.toml")]).unwrap();
        assert!(raw.status.success());
        assert!(
            String::from_utf8_lossy(&raw.stdout).contains(old.as_str()),
            "replacement must actually reinterpret the base tree"
        );
        let protected = base_pin(&root, floor);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            protected.as_deref(),
            Some(latest.as_str()),
            "the literal admitted base must keep its original tree"
        );
    }

    #[test]
    fn admission_immutable_authority_ignores_real_info_grafts() {
        let root = temp_path("admission-graft-authority");
        init_fixture_repo(&root);
        let a = commit_file(&root, "a", "a\n");
        let b = commit_file(&root, "b", "b\n");
        let tree = String::from_utf8(git_in(&root, &["rev-parse", "HEAD^{tree}"]).unwrap().stdout)
            .unwrap();
        let off = String::from_utf8(
            git_in(&root, &["commit-tree", tree.trim(), "-m", "unrelated"])
                .unwrap()
                .stdout,
        )
        .unwrap();
        let off = off.trim();
        fs::write(root.join(".git/info/grafts"), format!("{b} {off}\n{off}\n")).unwrap();
        assert!(
            git_in(&root, &["merge-base", "--is-ancestor", off, &b])
                .unwrap()
                .status
                .success(),
            "actual graft must change the unguarded ancestry answer"
        );
        let query = |ancestor: &str| {
            under_git_env(|| {
                authority_git_command()
                    .unwrap()
                    .arg("-C")
                    .arg(&root)
                    .args(["merge-base", "--is-ancestor", ancestor, &b])
                    .output()
                    .unwrap()
            })
        };
        let false_ancestry = query(off);
        let real_ancestry = query(&a);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            false_ancestry.status.code(),
            Some(1),
            "graft must not manufacture ancestry: {}",
            String::from_utf8_lossy(&false_ancestry.stderr)
        );
        assert!(
            real_ancestry.status.success(),
            "real original ancestry must remain accepted"
        );
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
        assert!(
            git_in(&root, &["add", "Cargo.toml"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&root, &["commit", "-qm", "head"])
                .unwrap()
                .status
                .success()
        );
        if staged_pin != head_pin {
            fs::write(root.join("Cargo.toml"), manifest(staged_pin)).expect("stage manifest");
            assert!(
                git_in(&root, &["add", "Cargo.toml"])
                    .unwrap()
                    .status
                    .success()
            );
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
            assert!(
                git_in(&remote, &["add", "revision"])
                    .unwrap()
                    .status
                    .success()
            );
            assert!(
                git_in(&remote, &["commit", "-qm", msg])
                    .unwrap()
                    .status
                    .success()
            );
        }
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );
        let tip = head(&remote);
        let first =
            String::from_utf8_lossy(&git_in(&remote, &["rev-parse", "main~2"]).unwrap().stdout)
                .trim()
                .to_string();
        let middle =
            String::from_utf8_lossy(&git_in(&remote, &["rev-parse", "main~1"]).unwrap().stdout)
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

    /// NEGATIVE LEG: the manifest whose OWN write fails must be restored, and named
    /// when it cannot be.
    ///
    /// `fs::write` truncates before writing, so the file that fails partway is the
    /// likeliest one in the tree to be damaged -- yet it never enters `written`, so
    /// a rollback over `written` alone skipped precisely it while the error text
    /// claimed the tree was no longer partially bumped. Here the failing manifest is
    /// read-only, so restoring it fails too and it must appear in the by-hand list.
    /// Before the fix that list was empty and the file went unmentioned entirely.
    #[test]
    fn a_manifest_whose_own_write_fails_is_restored_or_named() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_path("rollback-self");
        init_fixture_repo(&root);
        let old = "0123456789abcdef0123456789abcdef01234567";
        let latest = "89abcdef0123456789abcdef0123456789abcdef";
        let manifest = format!(
            "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{old}\" }}\n"
        );
        fs::create_dir_all(root.join("sub")).expect("create nested manifest dir");
        fs::write(root.join("Cargo.toml"), &manifest).expect("write first manifest");
        fs::write(root.join("sub/Cargo.toml"), &manifest).expect("write second manifest");
        assert!(git_in(&root, &["add", "."]).unwrap().status.success());

        // Make one manifest unwritable so its own fs::write fails.
        let locked = root.join("sub/Cargo.toml");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o444))
            .expect("make one manifest read-only");
        if fs::write(&locked, "probe").is_ok() {
            // Running with privileges that ignore the mode bit; the condition under
            // test cannot be produced here, so do not assert a false result.
            fs::write(&locked, &manifest).expect("restore probe write");
            fs::remove_dir_all(&root).expect("remove fixture repository");
            return;
        }

        let scan = read_pins(&root).expect("scan fixture manifests");
        let error = rewrite_manifest_pins(&scan, latest)
            .expect_err("an unwritable manifest must fail the all-or-nothing rewrite");
        assert!(
            error.contains("sub/Cargo.toml"),
            "the failing manifest must be named in the rollback accounting, got: {error}"
        );
        assert!(
            error.contains("restore by hand"),
            "a manifest that could not be restored must be named for manual repair, got: {error}"
        );

        // All-or-nothing still holds for the manifest that DID get written.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).expect("unlock");
        assert_eq!(
            fs::read_to_string(root.join("Cargo.toml")).expect("reread first manifest"),
            manifest,
            "a writable manifest must be rolled back to its original pin"
        );
        fs::remove_dir_all(&root).expect("remove fixture repository");
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
    fn exact_40_hex_excludes_a_longer_hex_run() {
        // ⚠️ THE REGRESSION THIS PINS. The DynamoRIO recipe key in the same file
        // is 64 hex characters. A length-40 match without an exact-length bound
        // takes its first 40 and reports a permanent false violation, which
        // would make the gate fire on every correct tree and be ignored inside a
        // week.
        let key = "key=sha256:132d77130980c546c8867fc196d97e664bc4816b1dfa9ea9c18de4a94d109c4d";
        assert!(exact_40_hex_tokens(key).is_empty(), "64-hex must not match");
    }

    #[test]
    fn exact_40_hex_finds_a_real_binding() {
        assert_eq!(
            exact_40_hex_tokens("expected_pin=86d9003a7a2a8d5399ef94a251e4d991d6c504a5"),
            vec!["86d9003a7a2a8d5399ef94a251e4d991d6c504a5".to_string()]
        );
        assert_eq!(
            exact_40_hex_tokens(
                "if [[ ${X:-} != ad598995c8018bf17414a92119acfac6c9fd58ee ]]; then"
            ),
            vec!["ad598995c8018bf17414a92119acfac6c9fd58ee".to_string()]
        );
    }

    #[test]
    fn exact_40_hex_ignores_short_tokens() {
        assert!(exact_40_hex_tokens("liteinst-runtime-build-7951770 abc123").is_empty());
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

    /// DEFECT 1 REGRESSION: a linked worktree of the product repository must be
    /// refused as a graph cache.
    ///
    /// Old-fails / new-passes: with the previous check -- cache `--git-dir`
    /// against product `--git-common-dir` -- a linked worktree reports
    /// `<common>/worktrees/<name>` for its git-dir, which never equals
    /// `<common>`, so `open` returned Ok and every subsequent `fetch --force`
    /// wrote into the PRODUCT repository's shared object store.
    #[test]
    fn guarded_repo_refuses_a_linked_worktree_of_the_product() {
        let base = std::env::temp_dir().join(format!(
            "reverie-pin-worktree-guard-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        let product = base.join("product");
        fs::create_dir_all(&product).expect("create product repository");
        assert!(
            git_in(&product, &["init", "-q"]).unwrap().status.success(),
            "init product"
        );
        assert!(
            git_in(&product, &["config", "user.email", "pin-test@example.com"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&product, &["config", "user.name", "Reverie Pin Test"])
                .unwrap()
                .status
                .success()
        );
        fs::write(product.join("seed"), "seed\n").expect("seed file");
        assert!(git_in(&product, &["add", "seed"]).unwrap().status.success());
        assert!(
            git_in(&product, &["commit", "-qm", "seed"])
                .unwrap()
                .status
                .success()
        );

        let linked = base.join("linked");
        let added = git_in(
            &product,
            &[
                "worktree",
                "add",
                "--detach",
                "-q",
                linked.to_str().expect("utf-8 worktree path"),
            ],
        )
        .expect("run git worktree add");
        assert!(
            added.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&added.stderr)
        );

        // Precondition that makes this test meaningful: the linked worktree's
        // git-dir really is DIFFERENT from the product common dir, which is
        // exactly why the old single comparison let it through.
        let linked_git_dir = canonical_git_path(&linked, "--git-dir").expect("linked git-dir");
        let product_common =
            canonical_git_path(&product, "--git-common-dir").expect("product common dir");
        assert_ne!(
            linked_git_dir, product_common,
            "precondition: a linked worktree's git-dir must differ from the common dir, \
             otherwise this test cannot discriminate the old behaviour"
        );

        let error = GuardedGitRepo::open(&linked, &product)
            .expect_err("a linked worktree of the product must be refused as a graph cache");
        assert!(
            error.contains("linked worktree"),
            "unexpected refusal text: {error}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// DEFECT 2 REGRESSION: repository-selecting variables must be cleared.
    ///
    /// Old-fails / new-passes for `git_root`/`git_in` is asserted structurally
    /// rather than by mutating the process environment, because these tests run
    /// in parallel threads and `set_var` would race every other test in the
    /// file.
    #[test]
    fn isolated_command_clears_repository_selecting_env() {
        let command = isolated_git_command().expect("build isolated git command");
        let removed: BTreeSet<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        for required in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
        ] {
            assert!(
                removed.contains(required),
                "{required} must be cleared for a command aimed at another repository; \
                 cleared set was {removed:?}"
            );
        }
    }

    /// DEFECT 3 REGRESSION: the caller's config overrides must survive.
    ///
    /// Old-fails / new-passes: the previous `clear_git_local_env` removed every
    /// name Git reported, and Git reports `GIT_CONFIG_COUNT` and
    /// `GIT_CONFIG_PARAMETERS` among them. Removing the count orphans the
    /// `GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` pairs, which Git never reports and
    /// so never removes -- the overrides disappear with nothing left to show it.
    /// On this fleet that silently drops the forward proxy's three
    /// `url.https://github.com/.insteadOf` rewrites.
    #[test]
    fn isolated_command_preserves_config_overrides() {
        let reported = git_local_env_vars().expect("enumerate git local env vars");
        let reported: BTreeSet<String> = reported
            .iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        // Precondition: without it this test proves nothing on a Git that does
        // not report the config names as local.
        assert!(
            reported.contains("GIT_CONFIG_COUNT") || reported.contains("GIT_CONFIG_PARAMETERS"),
            "precondition: this Git does not report config overrides as local env vars, \
             so the erasure this test guards cannot occur; reported: {reported:?}"
        );

        let command = isolated_git_command().expect("build isolated git command");
        let removed: BTreeSet<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        let kept: BTreeSet<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_some())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        for preserved in CONFIG_OVERRIDE_ENV_VARS {
            // Only a variable the parent actually sets can be observed as
            // preserved; removing one the parent never set is a no-op and
            // asserting on it would make this test depend on the shell.
            if env::var_os(preserved).is_none() {
                continue;
            }
            assert!(
                kept.contains(preserved) && !removed.contains(preserved),
                "{preserved} carries the caller's configuration, not a repository selection, \
                 and must survive isolation; kept {kept:?}, cleared {removed:?}"
            );
        }
    }

    /// FINDING 2 COMPANION: the identity guard must still be reachable when no
    /// symbolic link is involved, so that the new symlink refusal has not
    /// quietly become the only thing being tested.
    ///
    /// The alias here is a REAL DIRECTORY -- a linked worktree of the product
    /// created at the cache path -- so `resolve_cache_path` has nothing to
    /// refuse and the common-dir comparison is what stops it.
    #[test]
    fn reverie_graph_refuses_a_real_directory_aliasing_the_product() {
        let root = temp_path("real-alias-victim");
        let remote = temp_path("real-alias-foreign");
        init_fixture_repo(&root);
        init_fixture_repo(&remote);
        let victim_main = commit_file(&root, "victim", "Hermit\n");
        assert!(
            git_in(&root, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );
        commit_file(&remote, "foreign", "Reverie\n");
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );

        let cache = root.join("target/ci/reverie-graph.git");
        fs::create_dir_all(cache.parent().expect("cache parent")).expect("create cache parent");
        let added = git_in(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                "-q",
                cache.to_str().expect("UTF-8 cache path"),
            ],
        )
        .expect("run git worktree add");
        assert!(
            added.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&added.stderr)
        );
        assert!(
            !fs::symlink_metadata(&cache)
                .expect("stat cache")
                .file_type()
                .is_symlink(),
            "precondition: this fixture must not involve a symlink, or it would \
             exercise the wrong guard"
        );

        let error = reverie_graph(&root, remote.to_str().expect("UTF-8 fixture path"))
            .expect_err("a real directory aliasing the product must be refused");
        // A linked worktree carries a `.git` FILE, so the filesystem
        // classification now refuses it before any Git command is aimed at the
        // path -- strictly earlier than the identity guard that used to catch
        // it, and before `git init` rather than after. The identity guard has
        // not lost coverage: `guarded_repo_refuses_a_linked_worktree_of_the_product`
        // still names it directly, and
        // `guarded_repo_requires_the_cache_path_to_be_the_git_dir` covers the
        // path-identity half added with it.
        assert!(
            error.contains("REFUSING Reverie graph cache") && error.contains(".git file"),
            "refusal must name the .git-file carrier: {error}"
        );
        assert!(
            !cache.join("objects").exists(),
            "the refusal must fire BEFORE `git init --bare`, which would have created objects/"
        );
        assert_eq!(
            String::from_utf8_lossy(
                &git_in(&root, &["rev-parse", "refs/heads/main"])
                    .unwrap()
                    .stdout
            )
            .trim(),
            victim_main,
            "the refusal must fire before any foreign ref update"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&remote);
    }

    /// FINDING 1 REGRESSION: a distinct but NON-BARE cache must be refused.
    ///
    /// Distinctness from the product only proves it is a different repository,
    /// not a safe one. A cache with a worktree has a checked-out HEAD, and this
    /// script fetches `--force +refs/heads/main:refs/heads/main`, which moves a
    /// checked-out branch.
    #[test]
    fn guarded_repo_refuses_a_non_bare_cache() {
        let product = temp_path("nonbare-product");
        let cache = temp_path("nonbare-cache");
        init_fixture_repo(&product);
        commit_file(&product, "seed", "seed\n");
        init_fixture_repo(&cache);
        commit_file(&cache, "seed", "seed\n");

        // Precondition: distinct repositories, so only bareness can refuse it.
        let cache_common = canonical_git_path(&cache, "--git-common-dir").expect("cache common");
        let product_common =
            canonical_git_path(&product, "--git-common-dir").expect("product common");
        assert_ne!(
            cache_common, product_common,
            "precondition: the fixtures must be distinct repositories"
        );
        assert_eq!(
            String::from_utf8_lossy(
                &git_in(&cache, &["rev-parse", "--is-bare-repository"])
                    .unwrap()
                    .stdout
            )
            .trim(),
            "false",
            "precondition: the cache fixture must not be bare"
        );

        let error =
            GuardedGitRepo::open(&cache, &product).expect_err("a non-bare cache must be refused");
        assert!(
            error.contains("is not a bare repository"),
            "unexpected refusal text: {error}"
        );

        let _ = fs::remove_dir_all(&product);
        let _ = fs::remove_dir_all(&cache);
    }

    /// FINDING 2 REGRESSION, and the one that discriminates ORDER rather than
    /// outcome: nothing may be created through the link before validation.
    ///
    /// The other symlink fixture points at the product git-dir, where the
    /// identity guard would also have refused -- after `git init` had already
    /// run. This one points somewhere the identity guard has no opinion about,
    /// so the only thing that can stop a write is resolving before acting. The
    /// assertion is on the FILESYSTEM, not on the error text: the link target
    /// must still be empty afterwards.
    #[test]
    fn reverie_graph_creates_nothing_through_a_symlinked_cache() {
        use std::os::unix::fs::symlink;

        let root = temp_path("symlink-order-victim");
        let remote = temp_path("symlink-order-foreign");
        let elsewhere = temp_path("symlink-order-target");
        init_fixture_repo(&root);
        init_fixture_repo(&remote);
        commit_file(&root, "victim", "Hermit\n");
        commit_file(&remote, "foreign", "Reverie\n");
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );
        fs::create_dir_all(&elsewhere).expect("create link target");

        let cache = root.join("target/ci/reverie-graph.git");
        fs::create_dir_all(cache.parent().expect("cache parent")).expect("create cache parent");
        symlink(&elsewhere, &cache).expect("point cache outside the checkout");

        let error = reverie_graph(&root, remote.to_str().expect("UTF-8 fixture path"))
            .expect_err("a symlinked cache must be refused");
        assert!(
            error.contains("is a symbolic link"),
            "unexpected refusal text: {error}"
        );

        let created: Vec<_> = fs::read_dir(&elsewhere)
            .expect("read link target")
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .collect();
        assert!(
            created.is_empty(),
            "git init ran through the symlink before validation and created {created:?} in {}",
            elsewhere.display()
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&remote);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    // ================= THIRD PASS: four hardening items =================
    //
    // Each test below is paired with a reverted-fix run recorded in the commit
    // message. A test that only passes on the new code proves nothing about the
    // hole it claims to close.

    /// Serializes the tests that mutate this process's environment. Rust runs
    /// tests on threads of ONE process, so an unsynchronized `set_var` would
    /// leak into whatever else happened to be reading the environment.
    /// ⚠️ AN RwLock, NOT A Mutex, AND THE DISTINCTION IS THE FIX.
    ///
    /// A Mutex here serialised the WRITERS against each other and left the
    /// READERS unprotected -- and the readers are ordinary `git` invocations in
    /// other tests, which take no lock and cannot be made to. Rust runs tests as
    /// threads of ONE process, so `env::set_var` publishes to every thread at
    /// once; a child forked during the window between setting GIT_CONFIG_COUNT
    /// and setting GIT_CONFIG_KEY_0 inherits a count with no key.
    ///
    /// The partial state is real and observable -- a standalone harness with one
    /// writer doing exactly what `with_config_override` does and four threads
    /// shelling out to git saw it in 83 of 600 invocations.
    ///
    /// ⚠️ WHAT THAT PARTIAL STATE DOES: IT MAKES git FAIL, AND THIS COMMENT SAID
    /// THE OPPOSITE FOR THREE REVISIONS. Re-measured 2026-08-26 on the host
    /// recorded for this file in `docs/TESTING_ENVIRONMENTS.md` under "Named
    /// measurement hosts", both gits present there, inside a real repository,
    /// twenty runs each and no pipe between the command and `$?`:
    ///
    /// ```text
    /// env -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
    ///     GIT_CONFIG_COUNT=1 git rev-parse --show-toplevel
    ///   git 2.52.0        exit 128 in 20/20
    ///   git 2.53.0-Meta   exit 128 in 20/20
    ///   error: missing config key GIT_CONFIG_KEY_0
    ///   fatal: unable to parse command-line config
    /// POSITIVE CONTROL: GIT_CONFIG_COUNT=notanumber -> exit 128,
    ///   "error: bogus count in GIT_CONFIG_COUNT". It proves git PARSES the
    ///   variable, so the mechanism under test is live -- which is what saved a
    ///   review lane from filing a refutation of a correct result after measuring
    ///   0/20. It does NOT discriminate the environment: measured 2026-08-26 it
    ///   gives 128 with the variables present and 128 with them absent, exactly
    ///   like the control it replaced. NOTHING here needs to discriminate, because
    ///   with the `env -u` the recipe gives 20/20 either way; an earlier revision
    ///   of this line claimed otherwise and `agent(hermit-001)` and
    ///   `agent(hermit-015)` were right to refuse it.
    /// ```
    ///
    /// ⚠️ THE `env -u` IS LOAD-BEARING, NOT TIDINESS, AND OMITTING IT INVERTS THE
    /// RESULT -- FOR SOME READERS AND NOT OTHERS, WHICH IS THE WHOLE DIFFICULTY.
    /// Measured 2026-08-26 on the host named in `docs/TESTING_ENVIRONMENTS.md`:
    /// with `GIT_CONFIG_KEY_0` present the bare recipe gives 0 of 20, the exact
    /// opposite of the table above; with the `GIT_CONFIG*` family unset, 20 of 20.
    ///
    /// The variables are inherited from the AGENT HARNESS PROCESS, not from a git
    /// wrapper and not from shell initialisation. Agents launched with the proxy's
    /// GitHub URL-rewrite config carry `GIT_CONFIG_COUNT` plus `GIT_CONFIG_KEY_0..2`
    /// (`url.https://github.com/.insteadOf`) in their process environment before any
    /// shell starts, so `COUNT=1` finds an inherited `KEY_0` and git succeeds.
    ///
    /// ⚠️ SO A LOGIN SHELL PROVES NOTHING EITHER WAY, and that is why three
    /// reviewers disagreed while all measuring correctly. `bash -l` inherits from
    /// whatever launched it: from an affected agent it reports the variables, from
    /// an unaffected one it reports none, and NOTHING in `/etc/profile`,
    /// `/etc/profile.d`, `~/.bashrc`, `~/.bash_profile` or `~/.profile` mentions
    /// them -- grepped, zero hits. The same mechanism that lets git reach github.com
    /// through the proxy is the one that inverts this measurement.
    ///
    /// An earlier revision of this comment blamed "this host's git wrapper ...
    /// into every login shell". Both halves were false: no shell file sets these,
    /// and no executed program could have -- a child gets a COPY of the environment
    /// and cannot write back, so what the `git` on PATH is does not enter into it.
    /// Diagnosed by
    /// `agent(hermit-015)` and `agent(hermit-001)` on review, from
    /// `/proc/<parent>/environ`. Kept visible because this sentence is now on its
    /// fifth revision and a silent correction is what produced the previous four.
    ///
    /// ⚠️ SO THE REVISION THIS COMMENT "CORRECTED" WAS RIGHT. It said such a
    /// child "dies with `error: missing config key GIT_CONFIG_KEY_0`, exit 128".
    /// That is the exact string, verbatim, and it reproduces on both gits. The
    /// correction claimed it was "not reproducible here" and that "the only
    /// conditions that do exit 128 are an EMPTY key and a non-numeric count" --
    /// both of those DO exit 128, but they are not the only ones, and the
    /// sentence turned a true report into a false denial. A correction is not
    /// self-verifying merely because it is a correction.
    ///
    /// ⚠️ AND THIS STRENGTHENS THE LOCK RATHER THAN WEAKENING IT, WHICH IS WHY
    /// IT MATTERS. The read lock was defended by a comment saying the race it
    /// prevents is harmless. It is not: a child forked in the window between
    /// setting `GIT_CONFIG_COUNT` and setting `GIT_CONFIG_KEY_0` dies at 128
    /// rather than ignoring the surplus. Anyone who read this comment while
    /// deciding whether the lock earns its cost was reading an argument against
    /// it, assembled from a measurement nobody re-ran.
    ///
    /// The 0-of-30 figure is separate and stands: an earlier revision reported
    /// "8 failing tests across 8 runs" without the guard, and re-measured, the
    /// suite ran 30 times with no guard at all and failed 0 times. See
    /// [`under_git_env`] for the full table and the positive control.
    ///
    /// ⚠️ "Neither git there ... both" LOST ITS ANTECEDENT TO ONE OF THE FOUR
    /// DELETIONS, AND MY PREVIOUS NOTE HERE DENIED IT. I wrote that the phrase
    /// "had no antecedent and never did". `git log -S` says otherwise:
    ///
    ///   cec4602d32  + "<host>: ... Neither git ON THIS HOST rejects a"
    ///   89c06d0389  - "<host>: ... Neither git on this host rejects a"
    ///               + "on the measurement host: ... Neither git THERE"
    ///
    /// (the `<host>` elision is this gate's own doing -- see below.)
    ///
    /// "Neither git ON THIS HOST ... BOTH" parses, because the recorded host has
    /// two gits. `89c06d0389` -- which is hermit#2647, ONE OF THE FOUR DELETIONS this
    /// gate's failure message now indicts -- replaced the hostname and stranded
    /// `both`. So it is a FIFTH casualty of those commits and the strongest
    /// instance of the class: not a host string erased, but a SENTENCE whose
    /// meaning depended on one. The damage outlived the string it came from.
    /// ⚠️ AND I COULD NOT QUOTE THE DELETED LINE HERE, WHICH IS THE POINT MADE
    /// TWICE OVER. Spelling the hostname to show what was erased reddens
    /// `check-portable-paths.sh` -- I tripped it writing this paragraph, the
    /// third time tonight -- so the evidence for the erasure is itself elided,
    /// and the host lives in `docs/TESTING_ENVIRONMENTS.md` where the checker
    /// permits it. That is the correct remedy and it is also exactly the pressure
    /// that produced the four deletions: the cheapest way to satisfy the gate is
    /// to remove the identity rather than relocate it.
    ///
    /// Found by a review lane running `git log -S` on my claim rather than
    /// accepting it. Recorded because "the identity was deleted" and
    /// "the identity was never written down" look identical afterwards, and only
    /// one of them is tonight's story.
    ///
    /// ⚠️ AND THE CONCLUSION THAT ONCE FOLLOWED THAT 0-OF-30 IS FALSIFIED.
    /// Mutating `under_git_env` to a no-op FAILED 9 OF 10 RUNS at load average
    /// ~39, against 0 of 10 for the head as submitted, with the failing set
    /// varying between runs. The lock fixes REPRODUCED flakiness; it is not
    /// speculative hardening, and it must not be deleted as such. See
    /// [`under_git_env`].
    ///
    /// The write side takes the WRITE lock; the read side is taken by
    /// [`under_git_env`], and readers still run concurrently with each other --
    /// the file is not serialised.
    ///
    /// COVERAGE, counted rather than asserted. At the time of writing this file
    /// forks 27 child processes; 23 of them are `git` and 4 are not (`cargo` x2,
    /// `/bin/sh`, `env`, none of which read `GIT_CONFIG_*`). **22 of the 23 git
    /// forks run inside `under_git_env`.** The 23rd is the
    /// `git rev-parse --local-env-vars` call in `git_local_env_vars`, which runs
    /// UNDERNEATH the guard and so cannot take it; that one is made immune by
    /// stripping `GIT_CONFIG_COUNT` from the child instead. See the comment
    /// there.
    ///
    /// ⚠️ An earlier revision of this change guarded ONE of the 23 and said in a
    /// comment that it had guarded them all. Do not restate the coverage without
    /// recounting it: the failure mode of a partial guard is that it looks like
    /// a total one.
    static ENV_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

    /// Hold the read side across a git invocation, so no writer can publish a
    /// partial GIT_CONFIG_* set while this thread builds or forks.
    ///
    /// ⚠️ Call this only through [`under_git_env`]. Taking it directly invites
    /// the two mistakes that matter: holding it around the fork but not around
    /// the command construction, and holding it at a level that nests with
    /// another acquisition.
    pub(super) fn env_read_guard() -> std::sync::RwLockReadGuard<'static, ()> {
        ENV_LOCK
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Set the numbered Git config override variables, run `body`, and restore
    /// the previous values whatever happens.
    ///
    /// ⚠️ `body` MUST NOT INVOKE GIT. This holds the WRITE lock across `body`,
    /// and every git invocation in this file takes the READ lock through
    /// `under_git_env`; write-then-read on one thread is not reentrant and will
    /// hang the test with no message. Today both callers only read `GIT_CONFIG_*`
    /// through `env::var`, which is why this is a warning and not a bug.
    fn with_config_override<T>(pairs: &[(&str, &str)], body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut saved: Vec<(String, Option<OsString>)> = vec![(
            "GIT_CONFIG_COUNT".to_string(),
            env::var_os("GIT_CONFIG_COUNT"),
        )];
        env::set_var("GIT_CONFIG_COUNT", pairs.len().to_string());
        for (index, (key, value)) in pairs.iter().enumerate() {
            let key_var = format!("GIT_CONFIG_KEY_{index}");
            let value_var = format!("GIT_CONFIG_VALUE_{index}");
            saved.push((key_var.clone(), env::var_os(&key_var)));
            saved.push((value_var.clone(), env::var_os(&value_var)));
            env::set_var(&key_var, key);
            env::set_var(&value_var, value);
        }
        let outcome = body();
        for (name, previous) in saved {
            match previous {
                Some(value) => env::set_var(&name, value),
                None => env::remove_var(&name),
            }
        }
        outcome
    }

    /// ITEM 1, the decision. A rewrite that redirects the authority URL is
    /// detected; one that leaves it alone is not.
    ///
    /// The distinction is the whole design: the forward-proxy wrapper on this
    /// fleet sets `url.https://github.com/.insteadOf` rewrites, and refusing
    /// those would break every fetch. Only a rewrite that CHANGES the URL we
    /// are about to ask is an attack.
    #[test]
    fn only_a_rewrite_that_changes_the_authority_url_is_detected() {
        let remote = "https://github.com/rrnewton/reverie.git";

        // The attack, reproduced from the shell probe: redirect the whole URL.
        let attack = vec![(
            "https://github.com/rrnewton/reverie.git".to_string(),
            "/tmp/evil.git".to_string(),
        )];
        assert_eq!(
            rewritten_remote(remote, &attack).as_deref(),
            Some("/tmp/evil.git"),
            "a rewrite of the exact authority URL must be reported"
        );

        // A prefix rewrite is equally a redirection.
        let prefix = vec![(
            "https://github.com/".to_string(),
            "https://evil.example/".to_string(),
        )];
        assert_eq!(
            rewritten_remote(remote, &prefix).as_deref(),
            Some("https://evil.example/rrnewton/reverie.git")
        );

        // The proxy's shape: rewrites that map OTHER spellings INTO the
        // canonical URL. Our URL is already canonical, so nothing applies.
        let proxy = vec![
            (
                "git@github.com:".to_string(),
                "https://github.com/".to_string(),
            ),
            (
                "ssh://git@github.com/".to_string(),
                "https://github.com/".to_string(),
            ),
        ];
        assert_eq!(
            rewritten_remote(remote, &proxy),
            None,
            "a rewrite that does not touch the authority URL must be left alone"
        );

        // An identity rewrite changes nothing and must not be refused.
        let identity = vec![(
            "https://github.com/".to_string(),
            "https://github.com/".to_string(),
        )];
        assert_eq!(rewritten_remote(remote, &identity), None);

        // Git applies the LONGEST matching pattern, so this must too.
        let longest = vec![
            (
                "https://github.com/".to_string(),
                "https://short.example/".to_string(),
            ),
            (
                "https://github.com/rrnewton/".to_string(),
                "https://long.example/".to_string(),
            ),
        ];
        assert_eq!(
            rewritten_remote(remote, &longest).as_deref(),
            Some("https://long.example/reverie.git"),
            "the longest matching pattern wins, as in Git"
        );
    }

    /// ITEM 1, the wiring. The environment spelling of the attack is parsed
    /// out of `GIT_CONFIG_*` and refused by name.
    #[test]
    fn an_inherited_url_rewrite_of_the_authority_is_refused() {
        let remote = "https://github.com/rrnewton/reverie.git";
        let error = with_config_override(&[("url./tmp/evil.git.insteadOf", remote)], || {
            refuse_rewritten_authority_url(remote)
                .expect_err("a redirected authority URL must be refused")
        });
        assert!(
            error.contains("REFUSING to resolve the Reverie authority")
                && error.contains("/tmp/evil.git"),
            "the refusal must name the URL actually contacted: {error}"
        );

        // And the proxy's own shape must still pass.
        with_config_override(
            &[("url.https://github.com/.insteadOf", "git@github.com:")],
            || {
                refuse_rewritten_authority_url(remote)
                    .expect("a rewrite that does not touch the authority URL must be allowed")
            },
        );
    }

    /// ITEM 2. A replacement ref re-parents a commit, and that changes the
    /// ancestry answer this whole checker is built on.
    ///
    /// Measured on git 2.53 before writing the fix: with the replacement in
    /// place a plain `merge-base --is-ancestor` reports the off-history commit
    /// as an ancestor (rc 0), and reports it correctly again (rc 1) under
    /// `--no-replace-objects`. The assertion below is therefore not a
    /// tautology: the unguarded answer is checked in the same test and is the
    /// opposite one.
    #[test]
    fn graph_queries_refuse_a_replacement_ref_that_fakes_ancestry() {
        let cache = temp_path("replace-ref-cache");
        let product = temp_path("replace-ref-product");
        init_fixture_repo(&product);
        commit_file(&product, "p", "product\n");

        // A normal non-bare repo cannot be the cache, so build the fixture as a
        // bare repository and populate it by fetching from a worktree.
        let source = temp_path("replace-ref-source");
        init_fixture_repo(&source);
        commit_file(&source, "a", "A\n");
        let a = commit_file(&source, "b", "B\n");
        let b = commit_file(&source, "c", "C\n");
        // An off-history commit: same tree, no parent, so genuinely unrelated.
        let tree = String::from_utf8_lossy(
            &git_in(&source, &["rev-parse", "HEAD^{tree}"])
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        let off = String::from_utf8_lossy(
            &git_in(&source, &["commit-tree", &tree, "-m", "off"])
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        assert!(
            git_in(&source, &["replace", "--graft", &b, &off])
                .unwrap()
                .status
                .success(),
            "fixture must be able to install a replacement ref"
        );

        // The unguarded answer, in this same test, so the guarded one below is
        // demonstrably different rather than merely correct.
        // "Unguarded" here means without `--no-replace-objects`, which is the
        // point of the assertion. It still takes the ENVIRONMENT guard -- that
        // is a different mechanism and does not affect what this test measures.
        let unguarded = under_git_env(|| {
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["merge-base", "--is-ancestor", &off, &b])
                .status()
                .expect("run unguarded merge-base")
        });
        assert!(
            unguarded.success(),
            "precondition: without the guard the replacement must fake the ancestry, \
             or this test proves nothing"
        );

        fs::create_dir_all(&cache).expect("create cache dir");
        assert!(under_git_env(|| isolated_git_command()
            .unwrap()
            .args(["init", "--bare", "--quiet"])
            .arg(&cache)
            .status()
            .unwrap()
            .success()));
        // Bring the objects AND the replacement ref across, the way an attacker
        // who can write the cache would.
        assert!(
            under_git_env(|| isolated_git_command()
                .unwrap()
                .arg("--git-dir")
                .arg(&cache)
                .args(["fetch", "--quiet", "--no-tags"])
                .arg(&source)
                .args([
                    "+refs/heads/*:refs/heads/*",
                    "+refs/replace/*:refs/replace/*",
                ])
                .status()
                .unwrap()
                .success()),
            "fixture fetch must succeed"
        );

        let graph = GuardedGitRepo::open(&cache, &product).expect("the bare cache must be opened");
        assert!(
            !is_ancestor(&graph, &off, &b).expect("ancestry query must succeed"),
            "an off-history commit must NOT be certified as an ancestor merely because a \
             replacement ref re-parents the descendant"
        );
        // The honest control: real ancestry still answers yes, so the guard has
        // not simply disabled the query.
        assert!(
            is_ancestor(&graph, &a, &b).expect("ancestry query must succeed"),
            "a genuine ancestor must still be reported as one"
        );

        let _ = fs::remove_dir_all(&cache);
        let _ = fs::remove_dir_all(&product);
        let _ = fs::remove_dir_all(&source);
    }

    /// ITEM 3. A `.git` FILE at the cache path redirects every Git command to
    /// another git-dir. It must be refused from the filesystem, before `git
    /// init` has a chance to act through it.
    #[test]
    fn a_dot_git_file_carrier_is_refused_before_git_init() {
        let root = temp_path("dot-git-carrier-root");
        let elsewhere = temp_path("dot-git-carrier-target");
        init_fixture_repo(&root);
        commit_file(&root, "p", "product\n");
        fs::create_dir_all(&elsewhere).expect("create the redirect target");
        assert!(under_git_env(|| isolated_git_command()
            .unwrap()
            .args(["init", "--bare", "--quiet"])
            .arg(&elsewhere)
            .status()
            .unwrap()
            .success()));

        let cache = root.join("target/ci/reverie-graph.git");
        fs::create_dir_all(&cache).expect("create cache dir");
        fs::write(
            cache.join(".git"),
            format!("gitdir: {}\n", elsewhere.display()),
        )
        .expect("write the .git carrier");

        let error = classify_cache_path(&cache).expect_err("a .git-file carrier must be refused");
        assert!(
            error.contains("REFUSING Reverie graph cache") && error.contains(".git file"),
            "the refusal must name the carrier: {error}"
        );
        // The refusal is a pure filesystem decision, so nothing was created and
        // nothing was written through the pointer.
        assert!(
            !cache.join("objects").exists(),
            "classification must not create a repository at the cache path"
        );
        assert!(
            !elsewhere.join("refs/heads/main").exists(),
            "nothing may be written through the redirect target"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    /// ITEM 3. Any other nonempty, unrecognized cache is refused too, and an
    /// EMPTY directory is still accepted so a half-finished first run recovers.
    #[test]
    fn a_nonempty_unrecognized_cache_is_refused_and_an_empty_one_is_not() {
        let root = temp_path("unrecognized-cache-root");
        fs::create_dir_all(&root).expect("create root");

        let cache = root.join("cache.git");
        fs::create_dir_all(&cache).expect("create cache");
        assert!(
            matches!(
                classify_cache_path(&cache).expect("an empty directory is usable"),
                CacheState::Absent
            ),
            "an empty directory must remain usable, or a half-finished first run cannot recover"
        );

        fs::write(cache.join("someones-data.txt"), "not ours\n").expect("write foreign data");
        let error = classify_cache_path(&cache).expect_err("foreign contents must be refused");
        assert!(
            error.contains("REFUSING Reverie graph cache")
                && error.contains("not a bare repository")
                && error.contains("someones-data.txt"),
            "the refusal must name what it found: {error}"
        );
        assert!(
            cache.join("someones-data.txt").exists() && !cache.join("objects").exists(),
            "the foreign data must be left untouched and no repository created"
        );

        // A real bare repository at the path is recognized.
        let bare = root.join("bare.git");
        assert!(under_git_env(|| isolated_git_command()
            .unwrap()
            .args(["init", "--bare", "--quiet"])
            .arg(&bare)
            .status()
            .unwrap()
            .success()));
        assert!(matches!(
            classify_cache_path(&bare).expect("a bare repository is recognized"),
            CacheState::BareCache
        ));

        // A non-bare checkout carries a `.git` DIRECTORY and is refused.
        let checkout = root.join("checkout");
        init_fixture_repo(&checkout);
        let error = classify_cache_path(&checkout).expect_err("a checkout must be refused");
        assert!(
            error.contains(".git directory"),
            "a non-bare checkout must be named as such: {error}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// ITEM 4. The cache path, the git-dir and the common-dir must be one
    /// directory.
    ///
    /// The fixture is a directory carrying a `.git` FILE that names a BARE
    /// repository elsewhere. That shape is chosen deliberately: it passes every
    /// guard that existed before this item -- it is distinct from the product,
    /// and `--is-bare-repository` reports true through the pointer -- so the
    /// only thing that can refuse it is the comparison against the cache PATH.
    /// Confirmed by reverting the check: with it removed this test passes for
    /// the wrong reason, which is why the earlier non-bare fixture was replaced.
    ///
    /// `classify_cache_path` refuses this carrier earlier in the real flow;
    /// `open` is called directly here so the identity check is the thing under
    /// test rather than the filesystem classification.
    #[test]
    fn guarded_repo_requires_the_cache_path_to_be_the_git_dir() {
        let product = temp_path("path-identity-product");
        let cache = temp_path("path-identity-cache");
        let real = temp_path("path-identity-real");
        init_fixture_repo(&product);
        commit_file(&product, "p", "product\n");
        assert!(under_git_env(|| isolated_git_command()
            .unwrap()
            .args(["init", "--bare", "--quiet"])
            .arg(&real)
            .status()
            .unwrap()
            .success()));
        fs::create_dir_all(&cache).expect("create the carrier directory");
        fs::write(cache.join(".git"), format!("gitdir: {}\n", real.display()))
            .expect("write the .git carrier");

        // Precondition: the pointer really does redirect, and the target really
        // is bare -- so no earlier guard can be what refuses this.
        let reported = canonical_git_path(&cache, "--git-dir").expect("resolve the carrier");
        assert_eq!(
            reported,
            fs::canonicalize(&real).expect("canonicalize the real repository"),
            "precondition: the .git file must redirect to the bare repository"
        );
        assert_ne!(
            reported,
            fs::canonicalize(&cache).expect("canonicalize the cache path"),
            "precondition: the git-dir must differ from the cache path"
        );

        let error = GuardedGitRepo::open(&cache, &product)
            .expect_err("a path whose git-dir is elsewhere must be refused");
        assert!(
            error.contains("REFUSING Reverie graph cache") && error.contains("different directory"),
            "the refusal must name the path-identity failure: {error}"
        );

        let _ = fs::remove_dir_all(&product);
        let _ = fs::remove_dir_all(&cache);
        let _ = fs::remove_dir_all(&real);
    }

    /// ITEM 4. `core.bare=true` is a claim, not a fact. A repository that also
    /// configures `core.worktree` has a working tree and a checked-out HEAD,
    /// which is the hazard the bareness check exists to exclude.
    #[test]
    fn a_bare_claim_with_a_configured_worktree_is_refused() {
        let product = temp_path("fake-bare-product");
        let cache = temp_path("fake-bare-cache");
        let tree = temp_path("fake-bare-tree");
        init_fixture_repo(&product);
        commit_file(&product, "p", "product\n");
        fs::create_dir_all(&tree).expect("create the worktree");
        assert!(under_git_env(|| isolated_git_command()
            .unwrap()
            .args(["init", "--bare", "--quiet"])
            .arg(&cache)
            .status()
            .unwrap()
            .success()));
        assert!(
            under_git_env(|| isolated_git_command()
                .unwrap()
                .arg("--git-dir")
                .arg(&cache)
                .args(["config", "core.worktree"])
                .arg(&tree)
                .status()
                .unwrap()
                .success()),
            "fixture must be able to configure core.worktree"
        );

        let error = GuardedGitRepo::open(&cache, &product)
            .expect_err("a bare claim with a configured worktree must be refused");
        assert!(
            error.contains("core.worktree"),
            "the refusal must name the configured worktree: {error}"
        );

        let _ = fs::remove_dir_all(&product);
        let _ = fs::remove_dir_all(&cache);
        let _ = fs::remove_dir_all(&tree);
    }
}
