#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Tier 2 of the two-tier `Cargo.lock` gate (Reverie pin-flow design,
//! Finding 4): the omission lint. A change that touches any `Cargo.toml`
//! must also touch the owning Cargo root's `Cargo.lock`. There is no
//! attach-a-signal escape: a manifest edit whose lock genuinely does
//! not change regenerates to a no-op lock diff only by re-resolving,
//! and Tier 1 (`scripts/check-cargo-lock-freshness.rs`, the
//! `pre.cargo_lock` DAG node) independently proves freshness on every
//! run regardless. This catches "bumped the manifest, forgot the
//! lock" at diff time, in milliseconds, with no Cargo run — which is
//! the only place an fbsource-first edit can be caught, because the
//! exported lockfiles cannot be certified in place inside fbsource
//! (the internal workspace swaps roots and legitimately resolves
//! differently; see the design).
//!
//! Root ownership, in either tree shape (GitHub/export paths like
//! `reverie/reverie/Cargo.toml`, or fbsource paths like
//! `fbcode/hermetic_infra/reverie/reverie/Cargo.toml`): a manifest under
//! `reverie/reverie-liteinst/extract-harness/` (a standalone workspace)
//! is owned by its own `Cargo.lock`; any other manifest under
//! `reverie/` by `reverie/Cargo.lock`; under
//! `liteinst-runtime-build/` by `liteinst-runtime-build/Cargo.lock`;
//! any other Hermit manifest by the Hermit root `Cargo.lock`. One
//! co-owner rule sits on top for each nested workspace
//! (`liteinst-runtime-build`, `extract-harness`): it resolves Hermit
//! and Reverie packages by path, so its lockfile records those
//! packages' resolved dependencies. A changed manifest also requires
//! a nested lockfile when its package is in that resolution AND the
//! manifest no longer matches what the lock records for it (package
//! version, resolved dependency edges, version requirements, sources —
//! dev-dependencies and dependencies behind features the nested
//! workspace does not enable are not in the lock and never trigger
//! it, so every refusal here has a regeneration that satisfies it).
//! Without it, adding a dependency to such a package updates its
//! owning root lock while the nested lock silently goes stale (the
//! 2026-11-16 trunk break: `reverie-procfs` gained a
//! `reverie-proc-grammar` dependency, both root locks moved, the
//! nested lock did not, and every later `cargo metadata --locked`
//! on it exited 101). A `[workspace.dependencies]` edit is judged
//! the same way, by re-checking every member package in each nested
//! resolution against its lock.
//!
//! Which Reverie a nested lock records decides which manifests it is
//! judged against. Cargo resolves `liteinst-runtime-build/Cargo.lock`
//! only in a Hermit checkout, where `reverie/` is the submodule at
//! Hermit's pin; `extract-harness` lives inside Reverie and resolves
//! the Reverie beside it. In fbsource, `reverie/` is the in-tree
//! Reverie, which the pin follows only at the next repin, and Buck
//! never reads Hermit's nested lock. So with `--reverie-in-tree` (the
//! fbsource contract gate) a Reverie manifest does not oblige
//! `liteinst-runtime-build/Cargo.lock`, and that lock is judged here
//! only for the Hermit packages it records. Its other path packages
//! belong to the pinned Reverie, which this snapshot does not hold:
//! one with a Reverie manifest, or one no manifest names that a
//! recorded package depends on (the pin's packages are in the lock
//! only as dependencies). Every member of a nested workspace must be
//! recorded under its current name, so a renamed Hermit member still
//! refuses. Between a Reverie dependency change and the repin, no lock
//! could match both Reverie trees, so judging it against the in-tree
//! one refused a change that no regeneration could satisfy. The pinned
//! Reverie judges it instead: fbsource's Hermit CI job
//! (`fb-only/ci.sh`) materializes the pinned tree from this repository's
//! history (`fb-only/check-pinned-reverie-crates.py`, verified against
//! the pin record's digest) and runs `cargo metadata --locked` on
//! liteinst-runtime-build against it, so a repin, which rewrites the
//! pin record, is refused at its own diff if it does not carry the
//! lock's update. That refusal blocks the repin only when the Hermit CI
//! job runs on the repin's published version and is land-blocking
//! there, which fb-only/ci.sh notes is not yet established for that
//! job. Tier 1's `pre.cargo_lock` probe on the export-shaped
//! tree judges the same lock with the real submodule. Without the flag
//! (the export checkout), `reverie/` is the pin and every rule here
//! applies to it. Callers: the Makefile target in
//! either tree, changed files fed via `--files-from-stdin` where a
//! diff system supplies them, and the fbsource contract gate
//! (`fb-only/contract_checks`, `--check-tree` over a declared
//! snapshot), which is the diff-time refusal in fbsource.
//! Registration as an `arc lint` needs a DotSlash release through
//! Configerator before the engine entry can land, so it remains a
//! tracked follow-up; the rule lives here so both trees run one
//! implementation and one test suite.
//!
//! Changed files come from `--changed-from-git <base>` (`git diff
//! --name-only <base>` plus uncommitted changes when the base is a
//! commit) or from stdin with `--files-from-stdin`, one path per line.
//!
//! Scope limit, stated plainly: CI passes the change's parent
//! (`HEAD^`), so on a multi-commit branch this lint judges only the
//! last commit plus uncommitted work — a manifest-only commit buried
//! earlier in the branch is not re-judged here. That is acceptable
//! because Tier 1 (full `cargo metadata --locked` on every root, the
//! `pre.cargo_lock` DAG node) is the backstop: it refuses the stale
//! lock itself on every run, whichever commit introduced it. The
//! backstop has a hole of its own, stated here so nobody relies on
//! it silently: `pre.cargo_lock` carries the full/portable/
//! hosted-portable labels only, so it does not run in the quick or
//! super lanes — a buried manifest-only commit judged only by a
//! quick/super run is caught by neither tier until a full/portable
//! run (or its own last-commit lint) sees it. The nested co-owner
//! rule has limits of the same kind, stated plainly: membership is
//! read from the tree's nested lockfiles, so it judges packages
//! already in those resolutions; lockfiles record no feature sets,
//! so an edit that only rewires which optional dependencies a
//! feature enables is left to Tier 1; and on the GitHub export a
//! Reverie manifest change arrives as a submodule gitlink this
//! checker never sees — Tier 1's `pre.cargo_lock` probe on the
//! export-shaped tree (which covers both nested roots) is the
//! cover there. The static comparison is deliberately narrower than Cargo
//! resolution: it proves the nested lock still matches its
//! manifests' versions, edges and requirements, not that Cargo
//! would resolve to the same versions — Tier 1 remains the proof.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// The owning lockfiles, as Hermit-checkout-relative paths.
const HERMIT_LOCK: &str = "Cargo.lock";
const REVERIE_LOCK: &str = "reverie/Cargo.lock";
const LITEINST_LOCK: &str = "liteinst-runtime-build/Cargo.lock";
const EXTRACT_LOCK: &str = "reverie/reverie-liteinst/extract-harness/Cargo.lock";

/// Nested workspaces whose resolutions reach into the other roots by
/// path, so a manifest edit outside them can stale their lockfiles.
const NESTED_LOCKS: &[&str] = &[LITEINST_LOCK, EXTRACT_LOCK];

/// What the tree's `reverie/` is, which decides whether Reverie's
/// manifests are inputs of `liteinst-runtime-build/Cargo.lock`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ReverieSource {
    /// A Hermit checkout: `reverie/` is the submodule at Hermit's pin,
    /// the Reverie Cargo resolves Hermit's nested lock against.
    #[default]
    Pinned,
    /// fbsource: `reverie/` is the in-tree Reverie, which Hermit's pin
    /// follows only at the next repin.
    InTree,
}

/// Whether the nested lock `lock_path` records a different Reverie
/// from the tree's `reverie/`: only `liteinst-runtime-build/Cargo.lock`
/// in fbsource, where it records Hermit's pin (see the module docs).
fn records_pinned_reverie(reverie: ReverieSource, lock_path: &str) -> bool {
    reverie == ReverieSource::InTree && lock_path == LITEINST_LOCK
}

/// Whether the manifest at `normalized` is an input of the nested lock
/// `lock_path`: every manifest is, except a Reverie manifest for a lock
/// that records a different Reverie from this tree's.
fn is_nested_input(reverie: ReverieSource, lock_path: &str, normalized: &str) -> bool {
    let path = normalized.strip_prefix("hermit/").unwrap_or(normalized);
    !(records_pinned_reverie(reverie, lock_path) && path.starts_with("reverie/"))
}

#[derive(Default)]
struct Config {
    repo: Option<PathBuf>,
    base: Option<String>,
    files_from_stdin: bool,
    check_tree: bool,
    reverie: ReverieSource,
}

fn usage() -> &'static str {
    "Usage: check-cargo-lock-cochange.rs [OPTIONS]\n\
     \n\
     Refuse a change that edits a Cargo.toml without its owning root's\n\
     Cargo.lock. Remedy: regenerate and include the lockfile. CI passes\n\
     the change's parent as BASE (HEAD^ at the commit under test) so a\n\
     clean checkout still sees the change itself. With --check-tree,\n\
     refuse a tree whose nested lockfiles no longer match their\n\
     manifests (the fbsource contract gate; no changed set needed).\n\
     \n\
     Options:\n\
       --changed-from-git BASE   Changed files via `git diff --name-only BASE`\n\
       --files-from-stdin        Read changed paths from stdin, one per line\n\
       --check-tree              Judge the whole tree at --repo instead\n\
       --reverie-in-tree         reverie/ is fbsource's in-tree Reverie, not\n\
                                 Hermit's pin: Reverie manifests then do not\n\
                                 oblige liteinst-runtime-build/Cargo.lock\n\
       --repo PATH               Hermit checkout (default: git root)\n\
       -h, --help                Show this help"
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
            "--changed-from-git" => {
                config.base = Some(take_value(&args, &mut i, "--changed-from-git")?)
            }
            "--files-from-stdin" => config.files_from_stdin = true,
            "--check-tree" => config.check_tree = true,
            "--reverie-in-tree" => config.reverie = ReverieSource::InTree,
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
        i += 1;
    }
    let modes =
        config.base.is_some() as u32 + config.files_from_stdin as u32 + config.check_tree as u32;
    if modes != 1 {
        return Err(format!(
            "exactly one of --changed-from-git, --files-from-stdin or --check-tree is required\n{}",
            usage()
        ));
    }
    Ok(config)
}

/// Normalize a changed path from either tree shape to a
/// Hermit-checkout-relative path: strip everything through
/// `hermetic_infra/hermit/` or `hermetic_infra/` (the latter keeps the
/// `hermit/`/`reverie/` project segment, mapped below), and backslashes
/// never occur in these trees.
fn normalize_path(raw: &str) -> String {
    let path = raw.trim().trim_start_matches("./");
    if let Some(rest) = path.split("hermetic_infra/hermit/").nth(1) {
        if path.contains("hermetic_infra/hermit/") {
            return rest.to_string();
        }
    }
    if let Some(rest) = path.strip_prefix("hermetic_infra/") {
        // fbcode/hermetic_infra/reverie/... with the fbcode/ prefix
        // already stripped by the hermit/ arm's split fallback below.
        return rest.to_string();
    }
    if let Some(idx) = path.find("hermetic_infra/") {
        return path[idx + "hermetic_infra/".len()..].to_string();
    }
    path.to_string()
}

/// The owning root lockfile for a normalized manifest path, or `None`
/// when the path is not a manifest this lint owns. Paths here may still
/// carry the fbsource project segment (`hermit/...`, `reverie/...`)
/// after normalization; `hermit/` is peeled for the Hermit-owned cases.
fn owning_lock(normalized: &str) -> Option<&'static str> {
    if !normalized.ends_with("Cargo.toml") {
        return None;
    }
    let path = normalized.strip_prefix("hermit/").unwrap_or(normalized);
    if path.starts_with("reverie/reverie-liteinst/extract-harness/") {
        Some(EXTRACT_LOCK)
    } else if path.starts_with("reverie/") || path == "reverie/Cargo.toml" {
        Some(REVERIE_LOCK)
    } else if path.starts_with("liteinst-runtime-build/") {
        Some(LITEINST_LOCK)
    } else {
        Some(HERMIT_LOCK)
    }
}

/// Package name for a normalized manifest path, derived from its
/// directory: crate directories are named for their package in both
/// trees, with the one long-standing exception below. Workspace-root
/// manifests return `None`: they name no single package, and the
/// liteinst co-owner rule judges member packages only (see the scope
/// limit in the module docs). A future directory whose package name
/// differs under-fires this rule for that package until it is listed
/// here; it never over-fires.
fn manifest_package(normalized: &str) -> Option<String> {
    if !normalized.ends_with("/Cargo.toml") {
        return None;
    }
    let path = normalized.strip_prefix("hermit/").unwrap_or(normalized);
    let dir = path.strip_suffix("/Cargo.toml")?;
    match dir {
        // The `reverie` core crate: directory `reverie/reverie`,
        // package `reverie-core`.
        "reverie/reverie" => Some("reverie-core".to_string()),
        // The standalone harness workspace root is its one package.
        "reverie/reverie-liteinst/extract-harness" => Some("s5b-extract-harness".to_string()),
        "reverie" | "liteinst-runtime-build" => None,
        _ => dir.rsplit('/').next().map(str::to_string),
    }
}

/// Strip a `#` comment outside double quotes (TOML basics only;
/// lockfiles and these manifests need no more).
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' && in_string {
            escaped = true;
        } else if ch == '"' {
            in_string = !in_string;
        } else if ch == '#' && !in_string {
            return &line[..idx];
        }
    }
    line
}

/// Net bracket depth of a fragment, outside double quotes.
fn bracket_delta(text: &str) -> i32 {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            escaped = false;
        } else if ch == '\\' && in_string {
            escaped = true;
        } else if ch == '"' {
            in_string = !in_string;
        } else if !in_string {
            match ch {
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                _ => {}
            }
        }
    }
    depth
}

/// Logical TOML entries: physical lines joined until brackets
/// balance, comments stripped. Section headers are their own entry.
fn logical_entries(text: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if current.is_empty() && line.is_empty() {
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
        depth += bracket_delta(line);
        if depth <= 0 {
            entries.push(std::mem::take(&mut current));
            depth = 0;
        }
    }
    if !current.trim().is_empty() {
        entries.push(current);
    }
    entries
}

/// Unquote a `"..."` token, if it is one.
fn unquote(token: &str) -> Option<String> {
    token
        .trim()
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .map(str::to_string)
}

/// Every quoted string in a fragment (array elements, in order).
fn quoted_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '"' {
            continue;
        }
        let mut value = String::new();
        let mut escaped = false;
        for inner in chars.by_ref() {
            if escaped {
                value.push(inner);
                escaped = false;
            } else if inner == '\\' {
                escaped = true;
            } else if inner == '"' {
                break;
            } else {
                value.push(inner);
            }
        }
        out.push(value);
    }
    out
}

/// Split on commas at bracket depth zero, outside quotes.
fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0usize;
    for (idx, ch) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' && in_string {
            escaped = true;
        } else if ch == '"' {
            in_string = !in_string;
        } else if !in_string {
            match ch {
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(text[start..idx].to_string());
                    start = idx + 1;
                }
                _ => {}
            }
        }
    }
    parts.push(text[start..].to_string());
    parts
}

/// One resolved edge in a lockfile: `name`, with the locked version
/// when the lock annotates it (it does whenever a name resolves to
/// more than one version).
#[derive(Clone, Debug, PartialEq, Eq)]
struct LockedDep {
    name: String,
    version: Option<String>,
}

/// One `[[package]]` in a lockfile. `source` is `None` for path
/// packages — the Hermit/Reverie packages this lint judges — and
/// the recorded registry/git source otherwise.
#[derive(Clone, Debug)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
    deps: Vec<LockedDep>,
}

impl LockedPackage {
    fn is_path(&self) -> bool {
        self.source.is_none()
    }
}

/// A parsed `Cargo.lock`: every package, in file order.
#[derive(Clone, Debug)]
struct Lockfile {
    packages: Vec<LockedPackage>,
}

impl Lockfile {
    /// The path (sourceless) package with this name, if recorded.
    fn path_package(&self, name: &str) -> Option<&LockedPackage> {
        self.packages
            .iter()
            .find(|pkg| pkg.is_path() && pkg.name == name)
    }

    /// Whether a recorded package depends on a package with this name.
    fn is_depended_on(&self, name: &str) -> bool {
        self.packages
            .iter()
            .any(|pkg| pkg.deps.iter().any(|dep| dep.name == name))
    }
}

/// Parse a lock dependency string: `name`, `name version`, or
/// `name version (source)`.
fn parse_locked_dep(text: &str) -> LockedDep {
    let mut parts = text.split_whitespace();
    let name = parts.next().unwrap_or_default().to_string();
    let version = parts
        .next()
        .filter(|part| !part.starts_with('('))
        .map(str::to_string);
    LockedDep { name, version }
}

/// Parse a `Cargo.lock` text. Line-based on purpose: this lint stays
/// dependency-free and never runs Cargo, and lockfiles are
/// machine-written in this exact shape.
fn parse_lockfile(lock_text: &str) -> Lockfile {
    let mut packages: Vec<LockedPackage> = Vec::new();
    let mut current: Option<LockedPackage> = None;
    for entry in logical_entries(lock_text) {
        let entry = entry.trim();
        if entry.starts_with('[') {
            if let Some(pkg) = current.take() {
                packages.push(pkg);
            }
            if entry == "[[package]]" {
                current = Some(LockedPackage {
                    name: String::new(),
                    version: String::new(),
                    source: None,
                    deps: Vec::new(),
                });
            }
            continue;
        }
        let Some(pkg) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        match key.trim() {
            "name" if pkg.name.is_empty() => {
                if let Some(name) = unquote(value) {
                    pkg.name = name;
                }
            }
            "version" if pkg.version.is_empty() => {
                if let Some(version) = unquote(value) {
                    pkg.version = version;
                }
            }
            "source" => pkg.source = unquote(value),
            "dependencies" => {
                pkg.deps = quoted_strings(value)
                    .iter()
                    .map(|dep| parse_locked_dep(dep))
                    .collect();
            }
            _ => {}
        }
    }
    if let Some(pkg) = current.take() {
        packages.push(pkg);
    }
    packages.retain(|pkg| !pkg.name.is_empty());
    Lockfile { packages }
}

/// Package names recorded in a `Cargo.lock` text.
#[cfg(test)]
fn lockfile_packages(lock_text: &str) -> BTreeSet<String> {
    parse_lockfile(lock_text)
        .packages
        .iter()
        .map(|pkg| pkg.name.clone())
        .collect()
}

/// Where a declared dependency comes from. Compared against the
/// lock's recorded source: a re-vendor that only moves a git `rev`
/// changes no name or version, so source is part of the fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DepSource {
    Registry,
    Path,
    Git {
        url: String,
        rev: Option<String>,
        branch: Option<String>,
        tag: Option<String>,
    },
}

/// One declared dependency in a manifest. Dev-dependencies are
/// collected separately: a nested lock holds dev edges of its own
/// workspace members, but never of the path packages it resolves
/// from other roots, so callers choose per workspace.
#[derive(Clone, Debug)]
struct ManifestDep {
    /// Table key the dependency is declared under.
    key: String,
    /// Resolved package name (`package =` rename, else the key).
    package: String,
    req: Option<String>,
    optional: bool,
    inherited: bool,
    source: DepSource,
}

impl ManifestDep {
    fn new(key: &str) -> Self {
        let key = key.trim().trim_matches('"').to_string();
        ManifestDep {
            package: key.clone(),
            key,
            req: None,
            optional: false,
            inherited: false,
            source: DepSource::Registry,
        }
    }

    /// Apply one `field = value` of an inline table, a
    /// `[dependencies.<name>]` table, or a dotted key.
    fn apply(&mut self, field: &str, value: &str) {
        match field {
            "version" => self.req = unquote(value),
            "package" => {
                if let Some(package) = unquote(value) {
                    self.package = package;
                }
            }
            "optional" => self.optional = value.trim() == "true",
            "workspace" => self.inherited = value.trim() == "true",
            "path" => self.source = DepSource::Path,
            "git" => {
                if let Some(url) = unquote(value) {
                    let (rev, branch, tag) = match &self.source {
                        DepSource::Git {
                            rev, branch, tag, ..
                        } => (rev.clone(), branch.clone(), tag.clone()),
                        _ => (None, None, None),
                    };
                    self.source = DepSource::Git {
                        url,
                        rev,
                        branch,
                        tag,
                    };
                }
            }
            "rev" | "branch" | "tag" => {
                if let DepSource::Git {
                    rev, branch, tag, ..
                } = &mut self.source
                {
                    match field {
                        "rev" => *rev = unquote(value),
                        "branch" => *branch = unquote(value),
                        _ => *tag = unquote(value),
                    }
                } else if let Some(pin) = unquote(value) {
                    // Pin written before the git URL (dotted keys in
                    // any order): keep it on a placeholder URL the
                    // following `git` field overwrites.
                    self.source = DepSource::Git {
                        url: String::new(),
                        rev: (field == "rev").then_some(pin.clone()),
                        branch: (field == "branch").then_some(pin.clone()),
                        tag: (field == "tag").then_some(pin),
                    };
                }
            }
            _ => {}
        }
    }
}

/// The parts of a `Cargo.toml` this lint judges.
#[derive(Clone, Debug, Default)]
struct Manifest {
    name: Option<String>,
    version: Option<String>,
    version_inherited: bool,
    deps: Vec<ManifestDep>,
    dev_deps: Vec<ManifestDep>,
    workspace_deps: Vec<ManifestDep>,
    patches: Vec<ManifestDep>,
    workspace_version: Option<String>,
    members: Vec<String>,
}

/// Parse one dependency entry value: a version string, or an inline
/// table with `version`/`package`/`optional`/`workspace`/`path`/
/// `git`/`rev` keys.
fn parse_dep(key: &str, value: &str) -> ManifestDep {
    let value = value.trim();
    let mut dep = ManifestDep::new(key);
    if let Some(req) = unquote(value) {
        dep.req = Some(req);
        return dep;
    }
    let inner = value
        .strip_prefix('{')
        .and_then(|v| v.strip_suffix('}'))
        .unwrap_or(value);
    for part in split_top_level(inner) {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        dep.apply(k.trim(), v);
    }
    dep
}

/// Whether a section holds dependencies the nested locks resolve
/// (normal), dev-dependencies, or workspace-shared declarations.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DepKind {
    Normal,
    Dev,
    Workspace,
    Ignore,
}

/// Section path segments, split on dots outside single quotes
/// (`target.'cfg(...)'` keeps its quoted segment whole).
fn section_parts(section: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for ch in section.chars() {
        if ch == '\'' {
            in_quote = !in_quote;
        } else if ch == '.' && !in_quote {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    parts.push(current);
    parts
}

/// Classify a section: its dependency kind, plus the dependency
/// name when the section is a `[dependencies.<name>]` table (also
/// under `workspace.` and `target.'cfg(...)'.` prefixes).
fn dep_section(section: &str) -> (DepKind, Option<String>) {
    let parts = section_parts(section);
    let pos = parts.iter().position(|part| {
        matches!(
            part.as_str(),
            "dependencies" | "build-dependencies" | "dev-dependencies"
        )
    });
    let Some(pos) = pos else {
        return (DepKind::Ignore, None);
    };
    if parts.len() > pos + 2 {
        return (DepKind::Ignore, None);
    }
    let kind = if parts[..pos].iter().any(|part| part == "workspace") {
        DepKind::Workspace
    } else if parts[pos] == "dev-dependencies" {
        DepKind::Dev
    } else {
        DepKind::Normal
    };
    let table_dep = (parts.len() == pos + 2).then(|| parts[pos + 1].clone());
    (kind, table_dep)
}

/// Parse a manifest text (same line-based TOML subset as the lock
/// parser; these manifests are hand- and machine-written in it).
/// Dependency declarations in every spelling Cargo allows here —
/// string, inline table, `[dependencies.<name>]` table, and dotted
/// keys — merge into one entry per declared key.
fn parse_manifest(text: &str) -> Manifest {
    let mut manifest = Manifest::default();
    let mut section = String::new();
    // Keyed by (section, declared key): the same dependency declared
    // for several targets stays several declarations (their
    // requirements all bind), while fragments of one declaration
    // (inline table, [dependencies.<name>] table, dotted keys in one
    // section) merge into one entry.
    let mut deps: BTreeMap<String, ManifestDep> = BTreeMap::new();
    let mut dev_deps: BTreeMap<String, ManifestDep> = BTreeMap::new();
    let mut workspace_deps: BTreeMap<String, ManifestDep> = BTreeMap::new();
    let mut patches: BTreeMap<String, ManifestDep> = BTreeMap::new();
    for entry in logical_entries(text) {
        let entry = entry.trim();
        if entry.starts_with('[') {
            section = entry
                .trim_matches(|c| c == '[' || c == ']')
                .trim()
                .to_string();
            continue;
        }
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        let key = key.trim();
        match section.as_str() {
            "package" => match key {
                "name" => manifest.name = unquote(value),
                "version" => {
                    if value.trim().starts_with('{') {
                        manifest.version_inherited = parse_dep("version", value).inherited;
                    } else {
                        manifest.version = unquote(value);
                    }
                }
                _ => {}
            },
            "workspace" if key == "members" => {
                manifest.members = quoted_strings(value);
            }
            "workspace.package" if key == "version" => {
                manifest.workspace_version = unquote(value);
            }
            _ => {
                // [patch.<registry>] / [patch.<registry>.<name>]:
                // source replacements a nested root may declare.
                let parts = section_parts(&section);
                if parts.first().map(String::as_str) == Some("patch") {
                    if let Some(dep_key) = parts.get(2) {
                        patches
                            .entry(format!("{section}\0{dep_key}"))
                            .or_insert_with(|| ManifestDep::new(dep_key))
                            .apply(key, value);
                    } else if let Some((dep_key, field)) = key.split_once('.') {
                        patches
                            .entry(format!("{section}\0{dep_key}"))
                            .or_insert_with(|| ManifestDep::new(dep_key))
                            .apply(field.trim(), value);
                    } else {
                        let parsed = parse_dep(key, value);
                        patches.insert(format!("{section}\0{}", parsed.key), parsed);
                    }
                    continue;
                }
                let (kind, table_dep) = dep_section(&section);
                let map = match kind {
                    DepKind::Normal => &mut deps,
                    DepKind::Dev => &mut dev_deps,
                    DepKind::Workspace => &mut workspace_deps,
                    DepKind::Ignore => continue,
                };
                if let Some(dep_key) = table_dep {
                    map.entry(format!("{section}\0{dep_key}"))
                        .or_insert_with(|| ManifestDep::new(&dep_key))
                        .apply(key, value);
                } else if let Some((dep_key, field)) = key.split_once('.') {
                    map.entry(format!("{section}\0{dep_key}"))
                        .or_insert_with(|| ManifestDep::new(dep_key))
                        .apply(field.trim(), value);
                } else {
                    let parsed = parse_dep(key, value);
                    map.insert(format!("{section}\0{}", parsed.key), parsed);
                }
            }
        }
    }
    manifest.deps = deps.into_values().collect();
    manifest.dev_deps = dev_deps.into_values().collect();
    manifest.workspace_deps = workspace_deps.into_values().collect();
    manifest.patches = patches.into_values().collect();
    manifest
}

/// `(major, minor, patch)` plus how many components were written.
fn parse_version(text: &str) -> Option<(u64, u64, u64, usize)> {
    let core = text.trim().split(['-', '+']).next()?;
    let mut nums = [0u64; 3];
    let mut specified = 0usize;
    for (idx, part) in core.split('.').enumerate() {
        if idx >= 3 || part == "*" || part.eq_ignore_ascii_case("x") {
            break;
        }
        nums[idx] = part.parse().ok()?;
        specified += 1;
    }
    if specified == 0 {
        return None;
    }
    Some((nums[0], nums[1], nums[2], specified))
}

/// Whether `version` satisfies a Cargo version requirement, or
/// `None` when the requirement is outside the supported grammar
/// (callers fail closed). Supports caret (the default), tilde,
/// exact, comparisons, wildcards, and comma conjunctions — every
/// form the judged manifests use, unit-tested below.
fn req_satisfied(req: &str, version: &str) -> Option<bool> {
    let v = parse_version(version)?;
    let vt = (v.0, v.1, v.2);
    for clause in req.split(',') {
        let clause = clause.trim();
        if clause.is_empty() || clause == "*" {
            continue;
        }
        let (op, rest) = ["^", "~", ">=", "<=", "=", ">", "<"]
            .iter()
            .find_map(|op| clause.strip_prefix(op).map(|rest| (*op, rest.trim())))
            .unwrap_or(("^", clause));
        if rest == "*" {
            continue;
        }
        let (major, minor, patch, specified) = parse_version(rest)?;
        let base = (major, minor, patch);
        // A trailing wildcard component behaves like the exact
        // range at the precision before it (1.* == =1, 1.0.* == =1.0).
        let wildcard = rest.split(['-', '+']).next().is_some_and(|core| {
            core.split('.')
                .any(|c| c == "*" || c.eq_ignore_ascii_case("x"))
        });
        let ok = match (op, wildcard) {
            ("=", _) | (_, true) => match specified {
                1 => vt >= base && vt < (major + 1, 0, 0),
                2 => vt >= base && vt < (major, minor + 1, 0),
                _ => vt == base,
            },
            (">=", _) => vt >= base,
            ("<=", _) => vt <= base,
            (">", _) => vt > base,
            ("<", _) => vt < base,
            ("~", _) => {
                vt >= base
                    && vt
                        < if specified <= 1 {
                            (major + 1, 0, 0)
                        } else {
                            (major, minor + 1, 0)
                        }
            }
            _ => {
                // Caret: the first non-zero component may not move.
                let upper = if major > 0 {
                    (major + 1, 0, 0)
                } else if specified >= 2 && minor > 0 {
                    (0, minor + 1, 0)
                } else if specified >= 3 {
                    (0, 0, patch + 1)
                } else if specified == 2 {
                    (0, 1, 0)
                } else {
                    (1, 0, 0)
                };
                vt >= base && vt < upper
            }
        };
        if !ok {
            return Some(false);
        }
    }
    Some(true)
}

/// A dependency after workspace inheritance is applied. The same
/// package declared for several targets keeps every requirement
/// (the locked version must satisfy all of them) and every
/// distinct declared source.
struct ResolvedDep {
    reqs: Vec<String>,
    optional: bool,
    sources: Vec<DepSource>,
}

/// Resolve a manifest's dependencies to `package -> ResolvedDep`,
/// applying workspace inheritance from `workspace` (the parsed
/// workspace-root manifest, when read). Dev-dependencies join only
/// with `include_dev` (the package is a member of the nested
/// workspace being judged, whose lock records member dev edges).
/// An inherited name the workspace does not declare resolves to a
/// sentinel requirement that satisfies nothing, so the fingerprint
/// below reports it instead of passing silently.
fn resolved_deps(
    manifest: &Manifest,
    workspace: Option<&Manifest>,
    include_dev: bool,
) -> BTreeMap<String, ResolvedDep> {
    let mut resolved: BTreeMap<String, ResolvedDep> = BTreeMap::new();
    let declared = manifest.deps.iter().chain(
        include_dev
            .then_some(manifest.dev_deps.iter())
            .into_iter()
            .flatten(),
    );
    for dep in declared {
        let shared = if dep.inherited {
            workspace.and_then(|ws| {
                ws.workspace_deps
                    .iter()
                    .find(|ws_dep| ws_dep.key == dep.key)
            })
        } else {
            None
        };
        let package = shared
            .map(|s| s.package.clone())
            .unwrap_or_else(|| dep.package.clone());
        let resolved_dep = match (dep.inherited, shared) {
            (true, Some(shared)) => ResolvedDep {
                reqs: dep
                    .req
                    .clone()
                    .or_else(|| shared.req.clone())
                    .into_iter()
                    .collect(),
                optional: dep.optional || shared.optional,
                sources: vec![if dep.source == DepSource::Registry {
                    shared.source.clone()
                } else {
                    dep.source.clone()
                }],
            },
            (true, None) => ResolvedDep {
                reqs: vec!["=0.0.0".to_string()],
                optional: dep.optional,
                sources: vec![dep.source.clone()],
            },
            _ => ResolvedDep {
                reqs: dep.req.clone().into_iter().collect(),
                optional: dep.optional,
                sources: vec![dep.source.clone()],
            },
        };
        let entry = resolved.entry(package).or_insert_with(|| ResolvedDep {
            reqs: Vec::new(),
            optional: true,
            sources: Vec::new(),
        });
        // A package declared for several targets is required if any
        // declaration is, and the locked version must satisfy every
        // declaration's requirement from one of its declared sources.
        entry.optional = entry.optional && resolved_dep.optional;
        for req in resolved_dep.reqs {
            if !entry.reqs.contains(&req) {
                entry.reqs.push(req);
            }
        }
        for source in resolved_dep.sources {
            if !entry.sources.contains(&source) {
                entry.sources.push(source);
            }
        }
    }
    resolved
}

/// Whether a declared source matches the lock's recorded source.
/// Git identity is URL plus pin exactly as written: Cargo records
/// the declared rev/branch/tag verbatim in the lock source query
/// (`?rev=<declared>#<commit>`), so each must EQUAL the lock's
/// parameter, absent matching only absent. Prefix agreement is not
/// identity (a short rev and the full commit it abbreviates are
/// different spellings the next regeneration rewrites), and the
/// resolved commit fragment is never a substitute for a pin.
fn source_matches(declared: &DepSource, locked: Option<&str>) -> bool {
    match (declared, locked) {
        (DepSource::Path, None) => true,
        (DepSource::Registry, Some(source)) => {
            source.starts_with("registry+") || source.starts_with("sparse+")
        }
        (
            DepSource::Git {
                url,
                rev,
                branch,
                tag,
            },
            Some(source),
        ) => {
            let Some(rest) = source.strip_prefix("git+") else {
                return false;
            };
            let (before_fragment, _commit) = match rest.split_once('#') {
                Some((head, frag)) => (head, Some(frag)),
                None => (rest, None),
            };
            let (locked_url, query) = match before_fragment.split_once('?') {
                Some((url, query)) => (url, Some(query)),
                None => (before_fragment, None),
            };
            let normalize = |u: &str| {
                u.trim()
                    .trim_end_matches('/')
                    .strip_suffix(".git")
                    .map(str::to_string)
                    .unwrap_or_else(|| u.trim().trim_end_matches('/').to_string())
            };
            if normalize(url) != normalize(locked_url) {
                return false;
            }
            let param = |name: &str| {
                query.and_then(|query| {
                    query
                        .split('&')
                        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
                })
            };
            if param("rev") != rev.as_deref() {
                return false;
            }
            if param("branch") != branch.as_deref() {
                return false;
            }
            if param("tag") != tag.as_deref() {
                return false;
            }
            true
        }
        _ => false,
    }
}

/// Compare one manifest against what a nested lock records for its
/// package. `Some(reason)` means regeneration would move that lock
/// for this package (or the comparison itself could not be made and
/// fails closed); `None` means the lock already matches, so edits
/// the lock cannot record — dev-dependencies, dependencies behind
/// features it does not enable — correctly do not trigger it.
/// `include_dev` is set only for members of the nested workspace
/// being judged, whose dev edges the lock does record.
fn fingerprint_mismatch(
    manifest: &Manifest,
    workspace: Option<&Manifest>,
    locked: &LockedPackage,
    lock: &Lockfile,
    include_dev: bool,
    patches: &[ManifestDep],
) -> Option<String> {
    let version = if manifest.version_inherited {
        workspace.and_then(|ws| ws.workspace_version.clone())
    } else {
        manifest.version.clone()
    };
    if let Some(version) = version {
        if version != locked.version {
            return Some(format!(
                "package version {version} != locked {}",
                locked.version
            ));
        }
    }
    let resolved = resolved_deps(manifest, workspace, include_dev);
    let mut edges: Vec<&LockedDep> = locked.deps.iter().collect();
    edges.sort_by(|a, b| a.name.cmp(&b.name));
    for edge in edges {
        let Some(declared) = resolved.get(&edge.name) else {
            return Some(format!(
                "locked dependency {} is no longer declared",
                edge.name
            ));
        };
        // A [patch] in the nested root replaces this dependency's
        // source (and requirement, when it carries one) wholesale.
        let patch = patches.iter().find(|patch| patch.package == edge.name);
        let sources: Vec<DepSource> = match patch {
            Some(patch) => vec![patch.source.clone()],
            None => declared.sources.clone(),
        };
        let reqs: Vec<String> = match patch.and_then(|patch| patch.req.clone()) {
            Some(req) => vec![req],
            None if patch.is_some() => Vec::new(),
            None => declared.reqs.clone(),
        };
        // The edge passes when some locked candidate for it (the
        // annotated version, or any recorded version when the edge
        // carries none) matches a declared source and satisfies
        // every requirement. Cargo annotates the version whenever a
        // name resolves twice, so multi-candidate edges are a
        // robustness case, not an ambiguity to refuse.
        let candidates: Vec<&LockedPackage> = lock
            .packages
            .iter()
            .filter(|pkg| pkg.name == edge.name)
            .filter(|pkg| {
                edge.version
                    .as_ref()
                    .is_none_or(|version| version == &pkg.version)
            })
            .collect();
        if candidates.is_empty() {
            return Some(format!(
                "locked dependency {} has no package in the lock",
                edge.name
            ));
        }
        let mut first_reason: Option<String> = None;
        let mut edge_ok = false;
        for candidate in candidates {
            if !sources
                .iter()
                .any(|source| source_matches(source, candidate.source.as_deref()))
            {
                first_reason.get_or_insert_with(|| {
                    format!(
                        "source of {} no longer matches the lock ({:?})",
                        edge.name, candidate.source
                    )
                });
                continue;
            }
            let mut candidate_ok = true;
            for req in &reqs {
                match req_satisfied(req, &candidate.version) {
                    Some(true) => {}
                    Some(false) => {
                        first_reason.get_or_insert_with(|| {
                            format!(
                                "requirement {req:?} on {} excludes locked {}",
                                edge.name, candidate.version
                            )
                        });
                        candidate_ok = false;
                        break;
                    }
                    None => {
                        first_reason.get_or_insert_with(|| {
                            format!(
                                "requirement {req:?} on {} is outside the supported grammar",
                                edge.name
                            )
                        });
                        candidate_ok = false;
                        break;
                    }
                }
            }
            if candidate_ok {
                edge_ok = true;
                break;
            }
        }
        if !edge_ok {
            return first_reason
                .or_else(|| Some(format!("locked dependency {} does not match", edge.name)));
        }
    }
    let edge_names: BTreeSet<&str> = locked.deps.iter().map(|dep| dep.name.as_str()).collect();
    for (package, declared) in &resolved {
        if !declared.optional && !edge_names.contains(package.as_str()) {
            return Some(format!("new dependency {package} is not in the lock"));
        }
    }
    None
}

/// Candidate on-disk paths for a normalized tree path, in either
/// tree shape (export checkout root, or fbsource root).
fn tree_candidates(repo: &Path, normalized: &str) -> Vec<PathBuf> {
    let fbsource = if normalized.starts_with("reverie/") || normalized == "reverie/Cargo.toml" {
        repo.join("fbcode/hermetic_infra").join(normalized)
    } else {
        repo.join("fbcode/hermetic_infra/hermit").join(normalized)
    };
    vec![repo.join(normalized), fbsource]
}

/// Read a normalized tree path from the checkout, if present.
fn read_tree_file(repo: &Path, normalized: &str) -> Option<String> {
    tree_candidates(repo, normalized)
        .iter()
        .find_map(|candidate| std::fs::read_to_string(candidate).ok())
}

/// Load one lockfile from the checkout, in either tree shape. A
/// missing or unreadable lockfile is a checker error, never an
/// empty resolution: judging co-ownership without the resolution it
/// is derived from would silently restore the hole this rule closes.
fn load_lock(repo: &Path, lock: &str) -> Result<Lockfile, String> {
    let candidates = tree_candidates(repo, lock);
    for candidate in &candidates {
        match std::fs::read_to_string(candidate) {
            Ok(text) => return Ok(parse_lockfile(&text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!("could not read {}: {error}", candidate.display()));
            }
        }
    }
    Err(format!(
        "lockfile {lock} not found (tried {} and {}); the nested \
         co-owner rule reads its resolution from that lock",
        candidates[0].display(),
        candidates[1].display()
    ))
}

/// The workspace-root manifest (normalized path) a manifest inherits
/// shared declarations from.
fn workspace_root_of(normalized: &str) -> &'static str {
    let path = normalized.strip_prefix("hermit/").unwrap_or(normalized);
    if path.starts_with("liteinst-runtime-build/") {
        "liteinst-runtime-build/Cargo.toml"
    } else if path.starts_with("reverie/reverie-liteinst/extract-harness/") {
        "reverie/reverie-liteinst/extract-harness/Cargo.toml"
    } else if path.starts_with("reverie/") || path == "reverie/Cargo.toml" {
        "reverie/Cargo.toml"
    } else {
        "Cargo.toml"
    }
}

/// Package name for a manifest: the parsed `[package] name` when the
/// file can be read, else the directory convention (a deleted
/// manifest still names its package this way).
fn package_of(normalized: &str, text: Option<&str>) -> Option<String> {
    text.and_then(|text| parse_manifest(text).name)
        .or_else(|| manifest_package(normalized))
}

/// True when the changed set contains the given owning lockfile, in
/// either tree shape.
fn lock_changed(changed: &BTreeSet<String>, lock: &str) -> bool {
    changed.iter().any(|path| {
        let normalized = normalize_path(path);
        let peeled = normalized.strip_prefix("hermit/").unwrap_or(&normalized);
        peeled == lock
    })
}

/// The workspace-root manifest of a nested lock, normalized.
fn nested_root_manifest(lock: &str) -> &'static str {
    match lock {
        EXTRACT_LOCK => "reverie/reverie-liteinst/extract-harness/Cargo.toml",
        _ => "liteinst-runtime-build/Cargo.toml",
    }
}

/// Whether the manifest at `normalized` is a member of the nested
/// workspace `lock_path` belongs to (its dev edges are recorded).
fn is_nested_member(normalized: &str, lock_path: &str) -> bool {
    workspace_root_of(normalized) == nested_root_manifest(lock_path)
}

/// `[patch]` declarations of a nested workspace root (empty when
/// its manifest cannot be read; the judged manifest comparison
/// then proceeds unpatched, as before patches were judged).
fn nested_patches(lock_path: &str, read: &dyn Fn(&str) -> Option<String>) -> Vec<ManifestDep> {
    read(nested_root_manifest(lock_path))
        .map(|text| parse_manifest(&text).patches)
        .unwrap_or_default()
}

/// Whether one manifest (changed, at `normalized`) stales a nested
/// lock: its package is in that resolution and the manifest no
/// longer matches what the lock records for it. An unreadable
/// (deleted) manifest for a recorded package counts as stale.
fn nested_stale_for_manifest(
    normalized: &str,
    lock_path: &str,
    lock: &Lockfile,
    read: &dyn Fn(&str) -> Option<String>,
) -> bool {
    let text = read(normalized);
    let Some(package) = package_of(normalized, text.as_deref()) else {
        return false;
    };
    let Some(locked) = lock.path_package(&package) else {
        return false;
    };
    let Some(text) = text else {
        return true;
    };
    let manifest = parse_manifest(&text);
    let workspace = read(workspace_root_of(normalized)).map(|t| parse_manifest(&t));
    fingerprint_mismatch(
        &manifest,
        workspace.as_ref(),
        locked,
        lock,
        is_nested_member(normalized, lock_path),
        &nested_patches(lock_path, read),
    )
    .is_some()
}

/// Normalized member manifest paths of a workspace root, from its
/// `[workspace] members` (literal entries only; `None` when a glob
/// member cannot be enumerated without the filesystem listing this
/// lint does not do — callers fail closed).
fn member_manifests(root: &str, root_manifest: &Manifest) -> Option<Vec<String>> {
    let prefix = match root.rfind('/') {
        Some(idx) => &root[..idx + 1],
        None => "",
    };
    let mut paths = Vec::new();
    if root_manifest.name.is_some() {
        paths.push(root.to_string());
    }
    for member in &root_manifest.members {
        if member.contains('*') {
            return None;
        }
        if member == "." {
            paths.push(root.to_string());
        } else {
            paths.push(format!("{prefix}{member}/Cargo.toml"));
        }
    }
    paths.sort();
    paths.dedup();
    Some(paths)
}

/// Whether a workspace-root edit stales a nested lock: re-check
/// every member package in that resolution, since members inherit
/// the shared declarations the root edit may have moved.
fn root_stales_nested(
    root: &str,
    lock_path: &str,
    lock: &Lockfile,
    read: &dyn Fn(&str) -> Option<String>,
) -> bool {
    let Some(root_text) = read(root) else {
        return true;
    };
    let root_manifest = parse_manifest(&root_text);
    let Some(members) = member_manifests(root, &root_manifest) else {
        return true;
    };
    for member in members {
        let text = read(&member);
        let Some(package) = package_of(&member, text.as_deref()) else {
            continue;
        };
        let Some(locked) = lock.path_package(&package) else {
            continue;
        };
        let Some(text) = text else {
            return true;
        };
        let manifest = parse_manifest(&text);
        if fingerprint_mismatch(
            &manifest,
            Some(&root_manifest),
            locked,
            lock,
            is_nested_member(&member, lock_path),
            &nested_patches(lock_path, read),
        )
        .is_some()
        {
            return true;
        }
    }
    false
}

/// Apply the co-change rule to a changed-path set. Returns the list of
/// `(manifest, required lock)` violations, sorted for stable output.
/// A manifest missing two required locks appears twice, once per lock,
/// so the remedy names every lockfile to regenerate.
fn violations(
    reverie: ReverieSource,
    changed: &BTreeSet<String>,
    nested: &[(&'static str, &Lockfile)],
    read: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, &'static str)> {
    let mut found = Vec::new();
    for path in changed {
        let normalized = normalize_path(path);
        let Some(owner) = owning_lock(&normalized) else {
            continue;
        };
        if !lock_changed(changed, owner) {
            found.push((normalized.clone(), owner));
        }
        for (lock_path, lock) in nested {
            if *lock_path == owner
                || lock_changed(changed, lock_path)
                || !is_nested_input(reverie, lock_path, &normalized)
            {
                continue;
            }
            if nested_stale_for_manifest(&normalized, lock_path, lock, read) {
                found.push((normalized.clone(), lock_path));
            }
        }
    }
    for root in ["Cargo.toml", "reverie/Cargo.toml"] {
        let root_changed = changed.iter().any(|path| normalize_path(path) == root);
        if !root_changed {
            continue;
        }
        for (lock_path, lock) in nested {
            if !lock_changed(changed, lock_path)
                && is_nested_input(reverie, lock_path, root)
                && root_stales_nested(root, lock_path, lock, read)
            {
                found.push((root.to_string(), lock_path));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Every manifest in the tree a nested lock can resolve, as
/// `(normalized path, parsed manifest, workspace root)` — the
/// enumeration `--check-tree` fingerprints. Missing member
/// manifests and glob members are checker errors: a snapshot that
/// cannot name its inputs must fail loud, not scan less.
fn tree_manifests(
    read: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<(String, Manifest, String)>, String> {
    let roots = [
        "Cargo.toml",
        "reverie/Cargo.toml",
        "liteinst-runtime-build/Cargo.toml",
        "reverie/reverie-liteinst/extract-harness/Cargo.toml",
    ];
    let mut out = Vec::new();
    for root in roots {
        let Some(text) = read(root) else {
            return Err(format!("workspace root manifest not found: {root}"));
        };
        let root_manifest = parse_manifest(&text);
        let Some(members) = member_manifests(root, &root_manifest) else {
            return Err(format!(
                "workspace root {root} declares glob members this lint cannot enumerate"
            ));
        };
        for member in members {
            if member == root {
                out.push((member, root_manifest.clone(), root.to_string()));
                continue;
            }
            let Some(member_text) = read(&member) else {
                return Err(format!(
                    "workspace member manifest not found: {member} (from {root})"
                ));
            };
            out.push((member, parse_manifest(&member_text), root.to_string()));
        }
    }
    Ok(out)
}

/// `--check-tree`: refuse a tree whose nested lockfiles no longer
/// match their manifests, with the same fingerprint the co-change
/// rule uses. This is the fbsource contract gate's mode: Buck
/// supplies a declared snapshot instead of a changed set. Every member
/// of a nested workspace must be recorded in its lock under its current
/// name. With an in-tree Reverie, `liteinst-runtime-build/Cargo.lock` is
/// judged only for the Hermit packages it records; a path package with
/// a Reverie manifest, or with none that a recorded package depends on,
/// is the pinned Reverie's, which fb-only/ci.sh judges with Cargo.
fn check_tree(repo: &Path, reverie: ReverieSource) -> Result<i32, String> {
    let read = |path: &str| read_tree_file(repo, path);
    let locks: Vec<(&str, Lockfile)> = NESTED_LOCKS
        .iter()
        .map(|lock| Ok((*lock, load_lock(repo, lock)?)))
        .collect::<Result<_, String>>()?;
    let manifests = tree_manifests(&read)?;
    // Virtual workspace roots carry no [package], so they are not in
    // the manifest list; read them directly for inheritance.
    let roots: BTreeMap<String, Manifest> = [
        "Cargo.toml",
        "reverie/Cargo.toml",
        "liteinst-runtime-build/Cargo.toml",
        "reverie/reverie-liteinst/extract-harness/Cargo.toml",
    ]
    .iter()
    .filter_map(|root| read(root).map(|text| (root.to_string(), parse_manifest(&text))))
    .collect();
    let by_package: BTreeMap<String, usize> = manifests
        .iter()
        .enumerate()
        .filter_map(|(idx, (_path, manifest, _root))| manifest.name.clone().map(|name| (name, idx)))
        .collect();
    eprintln!(
        "Scope: checking {} manifest(s) against {} nested lockfile(s).",
        manifests.len(),
        locks.len()
    );
    let mut stale: Vec<String> = Vec::new();
    let mut judged = 0;
    let mut pinned = 0;
    for (lock_path, lock) in &locks {
        // A fresh lock records every member of its workspace by its
        // current name; a renamed member is caught only here, since the
        // loop below walks the lock's names.
        for (path, manifest, root) in &manifests {
            let Some(name) = &manifest.name else {
                continue;
            };
            if root.as_str() == nested_root_manifest(lock_path) && lock.path_package(name).is_none()
            {
                stale.push(format!(
                    "  {path}: workspace member {name} is not in {lock_path}"
                ));
            }
        }
        for pkg in lock.packages.iter().filter(|pkg| pkg.is_path()) {
            let Some(idx) = by_package.get(&pkg.name) else {
                // A package no manifest here names: the pinned Reverie's
                // (renamed or removed in-tree since the pin) when the lock
                // records the pin and a recorded package depends on it.
                if records_pinned_reverie(reverie, lock_path) && lock.is_depended_on(&pkg.name) {
                    pinned += 1;
                    continue;
                }
                stale.push(format!(
                    "  {} (in {lock_path}) has no manifest in the tree",
                    pkg.name
                ));
                continue;
            };
            let (path, manifest, root) = &manifests[*idx];
            if !is_nested_input(reverie, lock_path, path) {
                pinned += 1;
                continue;
            }
            judged += 1;
            if let Some(reason) = fingerprint_mismatch(
                manifest,
                roots.get(root),
                pkg,
                lock,
                root.as_str() == nested_root_manifest(lock_path),
                roots
                    .get(nested_root_manifest(lock_path))
                    .map(|root_manifest| root_manifest.patches.as_slice())
                    .unwrap_or(&[]),
            ) {
                stale.push(format!("  {path} vs {lock_path}: {reason}"));
            }
        }
    }
    if stale.is_empty() {
        if pinned == 0 {
            println!(
                "OK: every nested lockfile matches its manifests ({judged} path packages judged)."
            );
        } else {
            println!(
                "OK: every nested lockfile matches its manifests ({judged} path packages judged; \
                 {pinned} in {LITEINST_LOCK} belong to Hermit's pinned Reverie and are left to \
                 fb-only/ci.sh, which runs Cargo against the pinned tree)."
            );
        }
        return Ok(0);
    }
    loud_header("NESTED LOCKFILE STALE VS MANIFESTS - BLOCKED");
    for line in &stale {
        eprintln!("{line}");
    }
    eprintln!();
    eprintln!(
        "Regenerate the named lockfile(s) (`cargo metadata --format-version 1` \
         at that nested root) and include them. Tier 1 \
         (`cargo metadata --locked`) remains the full freshness proof."
    );
    Ok(1)
}

/// A git command whose repository comes only from `-C`/discovery, never
/// from inherited GIT_DIR-style variables (hooks and rebase steps export
/// them, and they would silently point this lint at another repository).
fn git_command() -> Command {
    let mut command = Command::new("git");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        command.env_remove(name);
    }
    command
}

fn git_root() -> Result<PathBuf, String> {
    let output = git_command()
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

fn changed_from_git(repo: &PathBuf, base: &str) -> Result<BTreeSet<String>, String> {
    let output = git_command()
        .arg("-C")
        .arg(repo)
        .args(["diff", "--name-only", base])
        .output()
        .map_err(|error| format!("could not run git diff: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git diff --name-only {base} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn loud_header(title: &str) {
    eprintln!("======================================================================");
    eprintln!("CARGO LOCK CO-CHANGE LINT: {title}");
    eprintln!("======================================================================");
}

fn run_with_config(config: Config) -> Result<i32, String> {
    let repo = config.repo.clone().map_or_else(git_root, Ok)?;
    if config.check_tree {
        return check_tree(&repo, config.reverie);
    }
    let changed = if config.files_from_stdin {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(|error| format!("could not read changed paths from stdin: {error}"))?;
        input
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        changed_from_git(&repo, config.base.as_deref().unwrap_or("HEAD"))?
    };
    let locks: Vec<(&str, Lockfile)> = NESTED_LOCKS
        .iter()
        .map(|lock| Ok((*lock, load_lock(&repo, lock)?)))
        .collect::<Result<_, String>>()?;
    let nested: Vec<(&str, &Lockfile)> = locks.iter().map(|(path, lock)| (*path, lock)).collect();
    let read = |path: &str| read_tree_file(&repo, path);
    eprintln!(
        "Scope: checking {} changed path(s) for Cargo.toml/Cargo.lock co-change \
         ({} nested lockfile(s) judged by resolved-edge fingerprint).",
        changed.len(),
        nested.len()
    );

    let found = violations(config.reverie, &changed, &nested, &read);
    if found.is_empty() {
        println!("OK: every changed Cargo.toml is accompanied by its required Cargo.lock(s).");
        return Ok(0);
    }
    loud_header("MANIFEST CHANGED WITHOUT ITS LOCKFILE - BLOCKED");
    for (manifest, lock) in &found {
        eprintln!("  {manifest} changed without {lock}");
    }
    eprintln!();
    eprintln!(
        "Regenerate and include the lockfile(s) above \
         (`cargo metadata --format-version 1` at that root). There is no \
         signal to attach instead: Tier 1 (pre.cargo_lock) proves freshness \
         on every run. Nested locks above were judged by resolved-edge \
         fingerprint, so each named regeneration moves that lock."
    );
    Ok(1)
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
    use super::*;

    fn changed(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    fn packages(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn no_files(_path: &str) -> Option<String> {
        None
    }

    fn reader(files: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'static {
        let map: BTreeMap<String, String> = files
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string()))
            .collect();
        move |path: &str| map.get(path).cloned()
    }

    /// The co-change rule as a Hermit checkout runs it, where
    /// `reverie/` is the submodule at Hermit's pin. Tests that judge
    /// an fbsource tree call `super::violations` with
    /// `ReverieSource::InTree`.
    fn violations(
        changed: &BTreeSet<String>,
        nested: &[(&'static str, &Lockfile)],
        read: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<(String, &'static str)> {
        super::violations(ReverieSource::Pinned, changed, nested, read)
    }

    /// A nested resolution containing none of the changed packages,
    /// so only the single-owner rule can fire: the pre-co-owner
    /// behavior, kept as the baseline the co-owner tests extend.
    fn no_overlap() -> Lockfile {
        parse_lockfile(
            "version = 4\n\n[[package]]\nname = \"some-unrelated-crate\"\nversion = \"0.0.0\"\n",
        )
    }

    #[test]
    fn manifest_only_change_is_refused_for_each_root() {
        let lock = no_overlap();
        let nested = [(LITEINST_LOCK, &lock)];
        assert_eq!(
            violations(&changed(&["detcore/Cargo.toml"]), &nested, &no_files),
            [("detcore/Cargo.toml".to_string(), HERMIT_LOCK)]
        );
        assert_eq!(
            violations(
                &changed(&["reverie/reverie/Cargo.toml"]),
                &nested,
                &no_files
            ),
            [("reverie/reverie/Cargo.toml".to_string(), REVERIE_LOCK)]
        );
        assert_eq!(
            violations(
                &changed(&["liteinst-runtime-build/runtime/Cargo.toml"]),
                &nested,
                &no_files
            ),
            [(
                "liteinst-runtime-build/runtime/Cargo.toml".to_string(),
                LITEINST_LOCK
            )]
        );
        assert_eq!(
            violations(
                &changed(&["reverie/reverie-liteinst/extract-harness/Cargo.toml"]),
                &nested,
                &no_files
            ),
            [(
                "reverie/reverie-liteinst/extract-harness/Cargo.toml".to_string(),
                EXTRACT_LOCK
            )]
        );
    }

    #[test]
    fn manifest_with_owning_lock_passes_and_lock_only_passes() {
        let lock = no_overlap();
        let nested = [(LITEINST_LOCK, &lock)];
        assert!(
            violations(
                &changed(&["detcore/Cargo.toml", "Cargo.lock"]),
                &nested,
                &no_files
            )
            .is_empty()
        );
        assert!(
            violations(
                &changed(&["reverie/reverie/Cargo.toml", "reverie/Cargo.lock"]),
                &nested,
                &no_files
            )
            .is_empty()
        );
        assert!(
            violations(
                &changed(&[
                    "liteinst-runtime-build/Cargo.toml",
                    "liteinst-runtime-build/Cargo.lock",
                ]),
                &nested,
                &no_files
            )
            .is_empty()
        );
        assert!(violations(&changed(&["Cargo.lock"]), &nested, &no_files).is_empty());
        assert!(violations(&changed(&["docs/Users.md"]), &nested, &no_files).is_empty());
        assert!(violations(&changed(&[]), &nested, &no_files).is_empty());
    }

    #[test]
    fn wrong_roots_lock_does_not_satisfy_the_rule() {
        // The Hermit root lock moving does not cover a Reverie manifest:
        // each root's lock is resolved independently.
        let lock = no_overlap();
        let nested = [(LITEINST_LOCK, &lock)];
        assert_eq!(
            violations(
                &changed(&["reverie/reverie-ptrace/Cargo.toml", "Cargo.lock"]),
                &nested,
                &no_files
            )
            .len(),
            1
        );
    }

    #[test]
    fn fbsource_path_shapes_map_to_the_same_roots() {
        let lock = no_overlap();
        let nested = [(LITEINST_LOCK, &lock)];
        assert_eq!(
            violations(
                &changed(&["fbcode/hermetic_infra/hermit/detcore/Cargo.toml"]),
                &nested,
                &no_files
            ),
            [("detcore/Cargo.toml".to_string(), HERMIT_LOCK)]
        );
        assert!(
            violations(
                &changed(&[
                    "fbcode/hermetic_infra/reverie/reverie/Cargo.toml",
                    "fbcode/hermetic_infra/reverie/Cargo.lock",
                ]),
                &nested,
                &no_files
            )
            .is_empty()
        );
        assert!(
            violations(
                &changed(&[
                    "fbcode/hermetic_infra/hermit/liteinst-runtime-build/Cargo.toml",
                    "fbcode/hermetic_infra/hermit/liteinst-runtime-build/Cargo.lock",
                ]),
                &nested,
                &no_files
            )
            .is_empty()
        );
    }

    /// The nested resolution before the 2026-11-16 break, reduced to
    /// the packages these tests judge.
    const NESTED_LOCK_TEXT: &str = "\
version = 4\n\
\n\
[[package]]\n\
name = \"detcore-model\"\n\
version = \"0.5.7\"\n\
dependencies = [\n\
 \"libc\",\n\
 \"reverie-core\",\n\
 \"reverie-procfs\",\n\
 \"reverie-syscalls\",\n\
 \"serde\",\n\
]\n\
\n\
[[package]]\n\
name = \"libc\"\n\
version = \"0.2.186\"\n\
source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
\n\
[[package]]\n\
name = \"reverie-core\"\n\
version = \"0.5.7\"\n\
dependencies = [\n\
 \"libc\",\n\
 \"reverie-syscalls\",\n\
]\n\
\n\
[[package]]\n\
name = \"reverie-inguest\"\n\
version = \"0.5.7\"\n\
dependencies = [\n\
 \"libc\",\n\
 \"reverie-syscalls\",\n\
 \"syscalls\",\n\
]\n\
\n\
[[package]]\n\
name = \"reverie-procfs\"\n\
version = \"0.5.7\"\n\
dependencies = [\n\
 \"libc\",\n\
 \"serde\",\n\
]\n\
\n\
[[package]]\n\
name = \"reverie-syscalls\"\n\
version = \"0.5.7\"\n\
dependencies = [\n\
 \"libc\",\n\
]\n\
\n\
[[package]]\n\
name = \"serde\"\n\
version = \"1.0.219\"\n\
source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
\n\
[[package]]\n\
name = \"syscalls\"\n\
version = \"0.8.1\"\n\
source = \"registry+https://github.com/rust-lang/crates.io-index\"\n";

    const HERMIT_ROOT_TEXT: &str = "\
[workspace]\n\
members = [\"detcore-model\"]\n\
\n\
[workspace.dependencies]\n\
reverie-core = { path = \"reverie/reverie\", version = \"0.5.7\" }\n\
reverie-proc-grammar = { path = \"reverie/reverie-proc-grammar\", version = \"0.5.7\" }\n\
reverie-procfs = { path = \"reverie/reverie-procfs\", version = \"0.5.7\" }\n\
reverie-syscalls = { path = \"reverie/reverie-syscalls\", version = \"0.5.7\" }\n";

    /// Red twin replaying the 2026-11-16 trunk break: reverie-procfs
    /// gained a reverie-proc-grammar dependency, both root locks
    /// moved, the nested lock did not. The single-owner rule passes
    /// that set; the fingerprint rule must refuse it by name, and
    /// pass it once the nested lock is included.
    #[test]
    fn liteinst_closure_manifest_also_requires_nested_lock() {
        let lock = parse_lockfile(NESTED_LOCK_TEXT);
        let nested = [(LITEINST_LOCK, &lock)];
        let read = reader(&[
            (
                "reverie/reverie-procfs/Cargo.toml",
                "[package]\nname = \"reverie-procfs\"\nversion = \"0.5.7\"\n\
                 \n[dependencies]\nlibc = \"0.2.186\"\n\
                 reverie-proc-grammar = { version = \"0.5.7\", path = \"../reverie-proc-grammar\" }\n\
                 serde = { version = \"1.0.219\", features = [\"derive\"] }\n",
            ),
            (
                "detcore-model/Cargo.toml",
                "[package]\nname = \"detcore-model\"\nversion = \"0.5.7\"\n\
                 \n[dependencies]\nlibc = \"0.2.186\"\n\
                 reverie-core = { workspace = true }\n\
                 reverie-proc-grammar = { workspace = true }\n\
                 reverie-syscalls = { workspace = true }\n\
                 serde = { version = \"1.0.219\", features = [\"derive\"] }\n",
            ),
            ("Cargo.toml", HERMIT_ROOT_TEXT),
            (
                "reverie/reverie/Cargo.toml",
                "[package]\nname = \"reverie-core\"\nversion = \"0.5.8\"\n\
                 \n[dependencies]\nlibc = \"0.2.186\"\n\
                 reverie-syscalls = { version = \"0.5.7\", path = \"../reverie-syscalls\" }\n",
            ),
        ]);
        let break_shape = changed(&[
            "fbcode/hermetic_infra/hermit/Cargo.lock",
            "fbcode/hermetic_infra/reverie/Cargo.lock",
            "fbcode/hermetic_infra/reverie/reverie-procfs/Cargo.toml",
        ]);
        assert_eq!(
            violations(&break_shape, &nested, &read),
            [(
                "reverie/reverie-procfs/Cargo.toml".to_string(),
                LITEINST_LOCK
            )]
        );
        let mut fixed = break_shape.clone();
        fixed.insert("fbcode/hermetic_infra/hermit/liteinst-runtime-build/Cargo.lock".to_string());
        assert!(
            violations(&fixed, &nested, &read).is_empty(),
            "nested lock included must satisfy the co-owner rule"
        );
        // Same shape from the Hermit side: detcore-model swapped its
        // procfs edge for grammar, which the nested lock lacks.
        assert_eq!(
            violations(
                &changed(&["detcore-model/Cargo.toml", "Cargo.lock"]),
                &nested,
                &read
            ),
            [("detcore-model/Cargo.toml".to_string(), LITEINST_LOCK)]
        );
        // The directory/package exception still maps into the closure
        // (here by a version bump the lock does not record).
        assert_eq!(
            violations(
                &changed(&["reverie/reverie/Cargo.toml", "reverie/Cargo.lock"]),
                &nested,
                &read
            ),
            [("reverie/reverie/Cargo.toml".to_string(), LITEINST_LOCK)]
        );
        // A Reverie package outside the nested resolution does not
        // pull the nested lock in: no false positive.
        assert!(
            violations(
                &changed(&["reverie/reverie-kvm/Cargo.toml", "reverie/Cargo.lock"]),
                &nested,
                &read
            )
            .is_empty()
        );
    }

    /// fbsource's in-tree Reverie is not the Reverie Hermit's nested
    /// lock resolves (that is Hermit's pin), so a Reverie manifest
    /// change there must not oblige liteinst-runtime-build/Cargo.lock:
    /// between the change and the repin no lock satisfies both trees.
    /// The same sets refuse in a Hermit checkout (the red half), and
    /// with an in-tree Reverie the rule still holds for Hermit's own
    /// manifests and for extract-harness, which resolves the Reverie
    /// beside it.
    #[test]
    fn in_tree_reverie_does_not_oblige_hermits_nested_lock() {
        let lock = parse_lockfile(NESTED_LOCK_TEXT);
        let liteinst = [(LITEINST_LOCK, &lock)];
        let staled_procfs = "[package]\nname = \"reverie-procfs\"\nversion = \"0.5.7\"\n\
             \n[dependencies]\nlibc = \"0.2.186\"\n\
             reverie-proc-grammar = { version = \"0.5.7\", path = \"../reverie-proc-grammar\" }\n\
             serde = { version = \"1.0.219\", features = [\"derive\"] }\n";
        let read = reader(&[
            ("reverie/reverie-procfs/Cargo.toml", staled_procfs),
            (
                "reverie/Cargo.toml",
                "[workspace]\nmembers = [\"reverie-procfs\"]\n",
            ),
            (
                "detcore-model/Cargo.toml",
                "[package]\nname = \"detcore-model\"\nversion = \"0.5.7\"\n\
                 \n[dependencies]\nlibc = \"0.2.186\"\n\
                 reverie-core = { workspace = true }\n\
                 reverie-proc-grammar = { workspace = true }\n\
                 reverie-syscalls = { workspace = true }\n\
                 serde = { version = \"1.0.219\", features = [\"derive\"] }\n",
            ),
            ("Cargo.toml", HERMIT_ROOT_TEXT),
        ]);
        let reverie_change = changed(&[
            "fbcode/hermetic_infra/reverie/Cargo.lock",
            "fbcode/hermetic_infra/reverie/reverie-procfs/Cargo.toml",
        ]);
        let reverie_root_edit = changed(&["reverie/Cargo.toml", "reverie/Cargo.lock"]);

        // Red half: a Hermit checkout, where reverie/ is the pin.
        assert_eq!(
            super::violations(ReverieSource::Pinned, &reverie_change, &liteinst, &read),
            [(
                "reverie/reverie-procfs/Cargo.toml".to_string(),
                LITEINST_LOCK
            )]
        );
        assert_eq!(
            super::violations(ReverieSource::Pinned, &reverie_root_edit, &liteinst, &read),
            [("reverie/Cargo.toml".to_string(), LITEINST_LOCK)]
        );

        // Green half: fbsource, where reverie/ is the in-tree Reverie.
        assert!(
            super::violations(ReverieSource::InTree, &reverie_change, &liteinst, &read).is_empty(),
            "an in-tree Reverie manifest must not oblige Hermit's nested lock"
        );
        assert!(
            super::violations(ReverieSource::InTree, &reverie_root_edit, &liteinst, &read)
                .is_empty(),
            "an in-tree Reverie root edit must not oblige Hermit's nested lock"
        );

        // Kept: Hermit's own manifest still obliges its nested lock.
        assert_eq!(
            super::violations(
                ReverieSource::InTree,
                &changed(&[
                    "fbcode/hermetic_infra/hermit/detcore-model/Cargo.toml",
                    "fbcode/hermetic_infra/hermit/Cargo.lock"
                ]),
                &liteinst,
                &read
            ),
            [("detcore-model/Cargo.toml".to_string(), LITEINST_LOCK)]
        );

        // Kept: extract-harness lives inside Reverie, so the in-tree
        // Reverie is its input (the same resolution stands in for it).
        let extract = [(EXTRACT_LOCK, &lock)];
        assert_eq!(
            super::violations(ReverieSource::InTree, &reverie_change, &extract, &read),
            [(
                "reverie/reverie-procfs/Cargo.toml".to_string(),
                EXTRACT_LOCK
            )]
        );
    }

    /// The seat's false fires, as green twins: edits the nested lock
    /// cannot record must not require it, because regenerating that
    /// lock is byte-identical and the refusal would be unsatisfiable.
    /// (The pre-fingerprint rule refused both sets below; the
    /// detcore-model one was also observed live against the real
    /// tree with a no-op regeneration.)
    #[test]
    fn dev_and_disabled_feature_edits_do_not_require_nested_lock() {
        let lock = parse_lockfile(NESTED_LOCK_TEXT);
        let nested = [(LITEINST_LOCK, &lock)];
        let read = reader(&[
            // detcore-model exactly as locked, plus a dev-dependency
            // the nested lock never records (pretty_assertions shape).
            (
                "detcore-model/Cargo.toml",
                "[package]\nname = \"detcore-model\"\nversion = \"0.5.7\"\n\
                 \n[dependencies]\nlibc = \"0.2.186\"\n\
                 reverie-core = { workspace = true }\n\
                 reverie-procfs = { workspace = true }\n\
                 reverie-syscalls = { workspace = true }\n\
                 serde = { version = \"1.0.219\", features = [\"derive\"] }\n\
                 \n[dev-dependencies]\n\
                 pretty_assertions = { version = \"1.4\", features = [\"alloc\"], default-features = false }\n",
            ),
            ("Cargo.toml", HERMIT_ROOT_TEXT),
            // reverie-inguest with a bumped optional dependency behind
            // features the nested workspace does not enable.
            (
                "reverie/reverie-inguest/Cargo.toml",
                "[package]\nname = \"reverie-inguest\"\nversion = \"0.5.7\"\n\
                 \n[features]\ndefault = [\"preload-constructor\"]\npreload-constructor = []\n\
                 exec-stop = [\"dep:nix\", \"dep:reverie-loader\"]\n\
                 \n[dependencies]\nlibc = \"0.2.186\"\nsyscalls = \"0.8.1\"\n\
                 nix = { version = \"0.31\", optional = true }\n\
                 reverie-loader = { version = \"0.5.7\", path = \"../reverie-loader\", optional = true }\n\
                 reverie-syscalls = { version = \"0.5.7\", path = \"../reverie-syscalls\" }\n",
            ),
        ]);
        assert!(
            violations(
                &changed(&["detcore-model/Cargo.toml", "Cargo.lock"]),
                &nested,
                &read
            )
            .is_empty(),
            "dev-dependency edit must not require the nested lock"
        );
        assert!(
            violations(
                &changed(&["reverie/reverie-inguest/Cargo.toml", "reverie/Cargo.lock"]),
                &nested,
                &read
            )
            .is_empty(),
            "disabled-feature dependency edit must not require the nested lock"
        );
        // Control: bumping a resolved requirement past the locked
        // version still fires on the same manifest.
        let bumped = reader(&[(
            "reverie/reverie-inguest/Cargo.toml",
            "[package]\nname = \"reverie-inguest\"\nversion = \"0.5.7\"\n\
             \n[dependencies]\nlibc = \"0.2.186\"\nsyscalls = \"0.9\"\n\
             reverie-syscalls = { version = \"0.5.7\", path = \"../reverie-syscalls\" }\n",
        )]);
        assert_eq!(
            violations(
                &changed(&["reverie/reverie-inguest/Cargo.toml", "reverie/Cargo.lock"]),
                &nested,
                &bumped
            ),
            [(
                "reverie/reverie-inguest/Cargo.toml".to_string(),
                LITEINST_LOCK
            )]
        );
    }

    #[test]
    fn workspace_dependencies_edit_is_judged_through_members() {
        let lock = parse_lockfile(NESTED_LOCK_TEXT);
        let nested = [(LITEINST_LOCK, &lock)];
        let detcore = (
            "detcore-model/Cargo.toml",
            "[package]\nname = \"detcore-model\"\nversion = \"0.5.7\"\n\
             \n[dependencies]\nlibc = \"0.2.186\"\n\
             reverie-core = { workspace = true }\n\
             reverie-procfs = { workspace = true }\n\
             reverie-syscalls = { workspace = true }\n\
             serde = { version = \"1.0.219\", features = [\"derive\"] }\n",
        );
        // Root-only edit that keeps every member fingerprint: green.
        let clean = reader(&[detcore, ("Cargo.toml", HERMIT_ROOT_TEXT)]);
        assert!(violations(&changed(&["Cargo.toml", "Cargo.lock"]), &nested, &clean).is_empty());
        // Root-only edit moving an inherited requirement past the
        // locked version: refused through the member it stales.
        let stale_root = HERMIT_ROOT_TEXT.replacen(
            "reverie-core = { path = \"reverie/reverie\", version = \"0.5.7\" }",
            "reverie-core = { path = \"reverie/reverie\", version = \"0.6\" }",
            1,
        );
        let stale = reader(&[detcore, ("Cargo.toml", &stale_root)]);
        assert_eq!(
            violations(&changed(&["Cargo.toml", "Cargo.lock"]), &nested, &stale),
            [("Cargo.toml".to_string(), LITEINST_LOCK)]
        );
    }

    #[test]
    fn extract_harness_lock_is_owned_and_co_owned() {
        let extract = parse_lockfile(NESTED_LOCK_TEXT);
        let nested = [(EXTRACT_LOCK, &extract)];
        let read = reader(&[(
            "reverie/reverie-inguest/Cargo.toml",
            "[package]\nname = \"reverie-inguest\"\nversion = \"0.5.7\"\n\
             \n[dependencies]\nlibc = \"0.2.186\"\nsyscalls = \"0.9\"\n\
             reverie-syscalls = { version = \"0.5.7\", path = \"../reverie-syscalls\" }\n",
        )]);
        assert_eq!(
            violations(
                &changed(&["reverie/reverie-inguest/Cargo.toml", "reverie/Cargo.lock"]),
                &nested,
                &read
            ),
            [(
                "reverie/reverie-inguest/Cargo.toml".to_string(),
                EXTRACT_LOCK
            )]
        );
    }

    /// N1 red/green twins: a git re-vendor moves only the pin.
    /// Names and versions are unchanged, so a fingerprint without
    /// source passes it (the V3 hole); with exact pin equality it
    /// must refuse each spelling change below, and pass once the
    /// lock records the declared spelling verbatim. Prefix
    /// agreement and the resolved-commit fallback (V4) passed the
    /// first four reds.
    #[test]
    fn git_rev_bump_requires_nested_lock() {
        const REV_A: &str = "4a6092fe5207866b35eb50c152cabcdc0bda0371";
        const REV_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        const SHORT: &str = "4a6092fe";
        const REV_C: &str = "4a6092fecccccccccccccccccccccccccccccccc";
        let lock_with_source = |suffix: &str| {
            parse_lockfile(&format!(
                "version = 4\n\
                 \n\
                 [[package]]\n\
                 name = \"liteinst-aarch64\"\n\
                 version = \"0.2.0\"\n\
                 source = \"git+https://github.com/rrnewton/liteinst2{suffix}\"\n\
                 \n\
                 [[package]]\n\
                 name = \"reverie-liteinst\"\n\
                 version = \"0.5.7\"\n\
                 dependencies = [\n\
                  \"liteinst-aarch64\",\n\
                 ]\n"
            ))
        };
        let manifest_with_pin = |pin: &str| {
            format!(
                "[package]\nname = \"reverie-liteinst\"\nversion = \"0.5.7\"\n\
                 \n[dependencies]\n\
                 liteinst-aarch64 = {{ version = \"0.2.0\", git = \"https://github.com/rrnewton/liteinst2\"{pin} }}\n"
            )
        };
        let changed_set = changed(&["reverie/reverie-liteinst/Cargo.toml", "reverie/Cargo.lock"]);
        let expect_blocked = [(
            "reverie/reverie-liteinst/Cargo.toml".to_string(),
            LITEINST_LOCK,
        )];
        // Full rev bump (the original re-vendor shape).
        let bumped = reader(&[(
            "reverie/reverie-liteinst/Cargo.toml",
            &manifest_with_pin(&format!(", rev = \"{REV_B}\"")),
        )]);
        assert_eq!(
            violations(
                &changed_set,
                &[(
                    LITEINST_LOCK,
                    &lock_with_source(&format!("?rev={REV_A}#{REV_A}"))
                )],
                &bumped
            ),
            expect_blocked,
            "rev bump must require the nested lock"
        );
        assert!(
            violations(
                &changed_set,
                &[(
                    LITEINST_LOCK,
                    &lock_with_source(&format!("?rev={REV_B}#{REV_B}"))
                )],
                &bumped
            )
            .is_empty(),
            "lock recording the new rev must satisfy the rule"
        );
        // (1) Short vs full spelling of the same commit: declared
        // short, lock records full. Regeneration rewrites the lock
        // query to the declared spelling, so this must fire.
        let short_declared = reader(&[(
            "reverie/reverie-liteinst/Cargo.toml",
            &manifest_with_pin(&format!(", rev = \"{SHORT}\"")),
        )]);
        assert_eq!(
            violations(
                &changed_set,
                &[(
                    LITEINST_LOCK,
                    &lock_with_source(&format!("?rev={REV_A}#{REV_A}"))
                )],
                &short_declared
            ),
            expect_blocked,
            "short declared rev vs full locked rev must fire"
        );
        assert!(
            violations(
                &changed_set,
                &[(
                    LITEINST_LOCK,
                    &lock_with_source(&format!("?rev={SHORT}#{REV_A}"))
                )],
                &short_declared
            )
            .is_empty(),
            "lock recording the short spelling verbatim must pass"
        );
        // (2) New commit sharing the short prefix: declared full C,
        // lock records the short spelling resolving to A. Prefix
        // matching in either direction passed this.
        let c_declared = reader(&[(
            "reverie/reverie-liteinst/Cargo.toml",
            &manifest_with_pin(&format!(", rev = \"{REV_C}\"")),
        )]);
        assert_eq!(
            violations(
                &changed_set,
                &[(
                    LITEINST_LOCK,
                    &lock_with_source(&format!("?rev={SHORT}#{REV_A}"))
                )],
                &c_declared
            ),
            expect_blocked,
            "full rev sharing the locked short prefix must fire"
        );
        // (3) Adding a pin to an unpinned lock: the commit fragment
        // is not a substitute for the declared rev.
        assert_eq!(
            violations(
                &changed_set,
                &[(LITEINST_LOCK, &lock_with_source(&format!("#{REV_B}")))],
                &bumped
            ),
            expect_blocked,
            "pin added against an unpinned lock must fire"
        );
        // (4) Removing a pin: declared plain git, lock still pinned.
        let unpinned = reader(&[(
            "reverie/reverie-liteinst/Cargo.toml",
            &manifest_with_pin(""),
        )]);
        assert_eq!(
            violations(
                &changed_set,
                &[(
                    LITEINST_LOCK,
                    &lock_with_source(&format!("?rev={REV_A}#{REV_A}"))
                )],
                &unpinned
            ),
            expect_blocked,
            "pin removed against a pinned lock must fire"
        );
        assert!(
            violations(
                &changed_set,
                &[(LITEINST_LOCK, &lock_with_source(&format!("#{REV_A}")))],
                &unpinned
            )
            .is_empty(),
            "unpinned lock recording no pin must pass the unpinned manifest"
        );
        // Branch/tag pins are exact too, absent matching only absent.
        let git = |rev: Option<&str>, branch: Option<&str>, tag: Option<&str>| DepSource::Git {
            url: "https://github.com/rrnewton/liteinst2".to_string(),
            rev: rev.map(str::to_string),
            branch: branch.map(str::to_string),
            tag: tag.map(str::to_string),
        };
        assert!(source_matches(
            &git(Some(REV_A), None, None),
            Some(&format!(
                "git+https://github.com/rrnewton/liteinst2?rev={REV_A}#{REV_A}"
            )),
        ));
        assert!(!source_matches(
            &git(None, Some("main"), None),
            Some(&format!(
                "git+https://github.com/rrnewton/liteinst2?branch=dev#{REV_A}"
            )),
        ));
        assert!(!source_matches(
            &git(None, None, None),
            Some(&format!(
                "git+https://github.com/rrnewton/liteinst2?tag=v1#{REV_A}"
            )),
        ));
        assert!(source_matches(
            &git(None, None, Some("v1")),
            Some(&format!(
                "git+https://github.com/rrnewton/liteinst2?tag=v1#{REV_A}"
            )),
        ));
    }

    /// N2 green twin at fingerprint level: a nested workspace
    /// member's dev edges ARE in its lock, so a correctly
    /// regenerated dev-dependency addition must pass; without
    /// member dev reading it refused forever. A dev requirement
    /// moved past the locked version still fires.
    #[test]
    fn member_dev_dependencies_are_judged_from_lock_edges() {
        let lock = parse_lockfile(
            "version = 4\n\
             \n\
             [[package]]\n\
             name = \"hermit-liteinst-runtime-artifact\"\n\
             version = \"0.5.7\"\n\
             dependencies = [\n\
              \"pretty_assertions\",\n\
             ]\n\
             \n\
             [[package]]\n\
             name = \"pretty_assertions\"\n\
             version = \"1.4.1\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        );
        let locked = lock
            .path_package("hermit-liteinst-runtime-artifact")
            .expect("fixture package")
            .clone();
        let matching = parse_manifest(
            "[package]\nname = \"hermit-liteinst-runtime-artifact\"\nversion = \"0.5.7\"\n\
             \n[dev-dependencies]\npretty_assertions = \"1.4\"\n",
        );
        assert_eq!(
            fingerprint_mismatch(&matching, None, &locked, &lock, true, &[]),
            None,
            "member dev edge as locked must pass"
        );
        assert!(
            fingerprint_mismatch(&matching, None, &locked, &lock, false, &[]).is_some(),
            "without member dev reading the same tree refused forever"
        );
        let bumped = parse_manifest(
            "[package]\nname = \"hermit-liteinst-runtime-artifact\"\nversion = \"0.5.7\"\n\
             \n[dev-dependencies]\npretty_assertions = \"1.5\"\n",
        );
        assert!(
            fingerprint_mismatch(&bumped, None, &locked, &lock, true, &[]).is_some(),
            "member dev requirement past the lock must fire"
        );
    }

    /// P3/P4 twins: a `[patch]` in the nested root replaces the
    /// source the lock records (without it, the patched tree refused
    /// forever); an edge naming a package locked at two versions
    /// passes when any candidate satisfies it; and a target-specific
    /// duplicate declaration keeps its own requirement.
    #[test]
    fn patch_ambiguous_and_target_duplicates_are_judged() {
        // [patch]: registry-declared serde, lock records the patched
        // path package (sourceless). Unpatched judging mismatches
        // forever; patched judging passes.
        let patched_lock = parse_lockfile(
            "version = 4\n\
             \n\
             [[package]]\n\
             name = \"detcore-model\"\n\
             version = \"0.5.7\"\n\
             dependencies = [\n\
              \"serde\",\n\
             ]\n\
             \n\
             [[package]]\n\
             name = \"serde\"\n\
             version = \"1.0.219\"\n",
        );
        let detcore = parse_manifest(
            "[package]\nname = \"detcore-model\"\nversion = \"0.5.7\"\n\
             \n[dependencies]\nserde = \"1.0.219\"\n",
        );
        let locked = patched_lock
            .path_package("detcore-model")
            .expect("fixture package")
            .clone();
        assert!(
            fingerprint_mismatch(&detcore, None, &locked, &patched_lock, false, &[]).is_some(),
            "patched source without patch awareness must mismatch"
        );
        let root_with_patch = parse_manifest(
            "[workspace]\nmembers = [\"runtime\"]\n\
             \n[package]\nname = \"hermit-liteinst-runtime-build\"\nversion = \"0.5.7\"\n\
             \n[patch.crates-io]\nserde = { path = \"vendor/serde\" }\n",
        );
        assert_eq!(
            fingerprint_mismatch(
                &detcore,
                None,
                &locked,
                &patched_lock,
                false,
                &root_with_patch.patches
            ),
            None,
            "patched source as locked must pass"
        );
        // Ambiguous edge: two locked versions, edge unannotated (a
        // shape Cargo does not emit, kept robust): any satisfying
        // candidate passes, none satisfying fires.
        let ambiguous_lock = parse_lockfile(
            "version = 4\n\
             \n\
             [[package]]\n\
             name = \"detcore-model\"\n\
             version = \"0.5.7\"\n\
             dependencies = [\n\
              \"serde\",\n\
             ]\n\
             \n\
             [[package]]\n\
             name = \"serde\"\n\
             version = \"1.0.100\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
             \n\
             [[package]]\n\
             name = \"serde\"\n\
             version = \"1.0.219\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        );
        let locked = ambiguous_lock
            .path_package("detcore-model")
            .expect("fixture package")
            .clone();
        assert_eq!(
            fingerprint_mismatch(&detcore, None, &locked, &ambiguous_lock, false, &[]),
            None,
            "a satisfying candidate must pass an unannotated edge"
        );
        // Target-specific duplicate: the stricter target requirement
        // binds even though the base declaration matches the lock.
        let lock = parse_lockfile(NESTED_LOCK_TEXT);
        let nested = [(LITEINST_LOCK, &lock)];
        let target_bump = reader(&[
            (
                "detcore-model/Cargo.toml",
                "[package]\nname = \"detcore-model\"\nversion = \"0.5.7\"\n\
                 \n[dependencies]\nlibc = \"0.2.186\"\n\
                 reverie-core = { workspace = true }\n\
                 reverie-procfs = { workspace = true }\n\
                 reverie-syscalls = { workspace = true }\n\
                 serde = { version = \"1.0.219\", features = [\"derive\"] }\n\
                 \n[target.'cfg(unix)'.dependencies]\nserde = \"1.1\"\n",
            ),
            ("Cargo.toml", HERMIT_ROOT_TEXT),
        ]);
        assert_eq!(
            violations(
                &changed(&["detcore-model/Cargo.toml", "Cargo.lock"]),
                &nested,
                &target_bump
            ),
            [("detcore-model/Cargo.toml".to_string(), LITEINST_LOCK)],
            "target-specific stricter requirement must fire"
        );
    }

    /// N3 twins: `[dependencies.<name>]` tables are judged like
    /// inline tables (red), and dotted keys — `foo.workspace = true`
    /// above all — parse as the dependency they declare (green),
    /// with a dotted version bump still firing (red control).
    #[test]
    fn table_and_dotted_dependency_spellings_are_judged() {
        let lock = parse_lockfile(NESTED_LOCK_TEXT);
        let nested = [(LITEINST_LOCK, &lock)];
        let table_form = reader(&[(
            "reverie/reverie-procfs/Cargo.toml",
            "[package]\nname = \"reverie-procfs\"\nversion = \"0.5.7\"\n\
             \n[dependencies]\nlibc = \"0.2.186\"\n\
             serde = { version = \"1.0.219\", features = [\"derive\"] }\n\
             \n[dependencies.reverie-proc-grammar]\n\
             version = \"0.5.7\"\npath = \"../reverie-proc-grammar\"\n",
        )]);
        assert_eq!(
            violations(
                &changed(&["reverie/reverie-procfs/Cargo.toml", "reverie/Cargo.lock"]),
                &nested,
                &table_form
            ),
            [(
                "reverie/reverie-procfs/Cargo.toml".to_string(),
                LITEINST_LOCK
            )],
            "table-form new dependency must require the nested lock"
        );
        let dotted = reader(&[
            (
                "detcore-model/Cargo.toml",
                "[package]\nname = \"detcore-model\"\nversion = \"0.5.7\"\n\
                 \n[dependencies]\nlibc = \"0.2.186\"\n\
                 reverie-core.workspace = true\n\
                 reverie-procfs.workspace = true\n\
                 reverie-syscalls.workspace = true\n\
                 serde.version = \"1.0.219\"\nserde.features = [\"derive\"]\n",
            ),
            ("Cargo.toml", HERMIT_ROOT_TEXT),
        ]);
        assert!(
            violations(
                &changed(&["detcore-model/Cargo.toml", "Cargo.lock"]),
                &nested,
                &dotted
            )
            .is_empty(),
            "dotted workspace keys matching the lock must pass"
        );
        let dotted_bump = reader(&[
            (
                "detcore-model/Cargo.toml",
                "[package]\nname = \"detcore-model\"\nversion = \"0.5.7\"\n\
                 \n[dependencies]\nlibc = \"0.2.186\"\n\
                 reverie-core.workspace = true\n\
                 reverie-procfs.workspace = true\n\
                 reverie-syscalls.workspace = true\n\
                 serde.version = \"1.1\"\n",
            ),
            ("Cargo.toml", HERMIT_ROOT_TEXT),
        ]);
        assert_eq!(
            violations(
                &changed(&["detcore-model/Cargo.toml", "Cargo.lock"]),
                &nested,
                &dotted_bump
            ),
            [("detcore-model/Cargo.toml".to_string(), LITEINST_LOCK)],
            "dotted version bump past the lock must fire"
        );
    }

    #[test]
    fn req_satisfied_covers_cargo_forms() {
        for (req, version, want) in [
            ("0.5.7", "0.5.7", true),
            ("0.5.7", "0.5.8", true),
            ("0.5.7", "0.6.0", false),
            ("1.0.219", "1.0.229", true),
            ("1", "1.54.1", true),
            ("2", "2.0.1", true),
            ("2", "3.0.0", false),
            ("=0.10.7", "0.10.7", true),
            ("=0.10.7", "0.10.9", false),
            ("~1.2.3", "1.2.9", true),
            ("~1.2.3", "1.3.0", false),
            (">=0.8.1, <1", "0.8.1", true),
            ("1.*", "1.9.0", true),
            ("*", "9.9.9", true),
        ] {
            assert_eq!(
                req_satisfied(req, version),
                Some(want),
                "{req} vs {version}"
            );
        }
        assert_eq!(req_satisfied("not-a-requirement", "1.0.0"), None);
    }

    #[test]
    fn missing_nested_lock_fails_closed() {
        use std::fs;
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let repo = env::temp_dir().join(format!(
            "cargo-lock-cochange-missing-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&repo);
        fs::create_dir_all(&repo).expect("fixture dir");
        let error = load_lock(&repo, LITEINST_LOCK).expect_err("must refuse");
        assert!(
            error.contains("not found"),
            "missing lock must fail closed, got: {error}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn check_tree_judges_a_snapshot_without_a_changed_set() {
        use std::fs;
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let repo = env::temp_dir().join(format!(
            "cargo-lock-cochange-tree-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&repo);
        let write = |rel: &str, text: &str| {
            let path = repo.join(rel);
            fs::create_dir_all(path.parent().expect("parent")).unwrap();
            fs::write(path, text).unwrap();
        };
        write("Cargo.toml", "[workspace]\nmembers = [\"detcore-model\"]\n");
        write(
            "detcore-model/Cargo.toml",
            "[package]\nname = \"detcore-model\"\nversion = \"0.5.7\"\n\
             \n[dependencies]\nlibc = \"0.2.186\"\n",
        );
        write("reverie/Cargo.toml", "[workspace]\nmembers = []\n");
        write(
            "liteinst-runtime-build/Cargo.toml",
            "[workspace]\nmembers = [\"runtime\"]\n\
             \n[package]\nname = \"hermit-liteinst-runtime-build\"\nversion = \"0.5.7\"\n",
        );
        write(
            "liteinst-runtime-build/runtime/Cargo.toml",
            "[package]\nname = \"hermit-liteinst-runtime-artifact\"\nversion = \"0.5.7\"\n",
        );
        write(
            "reverie/reverie-liteinst/extract-harness/Cargo.toml",
            "[package]\nname = \"s5b-extract-harness\"\nversion = \"0.0.0\"\n\
             \n[workspace]\n\n[dependencies]\nlibc = \"0.2.186\"\n",
        );
        let liteinst_lock = "\
version = 4\n\
\n\
[[package]]\n\
name = \"detcore-model\"\n\
version = \"0.5.7\"\n\
dependencies = [\n\
 \"libc\",\n\
]\n\
\n\
[[package]]\n\
name = \"hermit-liteinst-runtime-artifact\"\n\
version = \"0.5.7\"\n\
\n\
[[package]]\n\
name = \"hermit-liteinst-runtime-build\"\n\
version = \"0.5.7\"\n\
\n\
[[package]]\n\
name = \"libc\"\n\
version = \"0.2.186\"\n\
source = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        write("liteinst-runtime-build/Cargo.lock", liteinst_lock);
        write(
            "reverie/reverie-liteinst/extract-harness/Cargo.lock",
            "version = 4\n\n[[package]]\nname = \"libc\"\nversion = \"0.2.186\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
             \n[[package]]\nname = \"s5b-extract-harness\"\nversion = \"0.0.0\"\n\
             dependencies = [\n \"libc\",\n]\n",
        );
        assert_eq!(
            check_tree(&repo, ReverieSource::Pinned).expect("tree check runs"),
            0,
            "matching snapshot must pass"
        );
        write(
            "detcore-model/Cargo.toml",
            "[package]\nname = \"detcore-model\"\nversion = \"0.5.8\"\n\
             \n[dependencies]\nlibc = \"0.2.186\"\n",
        );
        assert_eq!(
            check_tree(&repo, ReverieSource::Pinned).expect("tree check runs"),
            1,
            "stale snapshot must refuse"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// The fbsource gate's shape of the reverie-memory zerocopy
    /// change: the in-tree Reverie gains a dependency that Hermit's
    /// pin does not have yet, so Hermit's nested lock (which records
    /// the pin) lacks it. A Hermit checkout refuses that tree (red);
    /// fbsource passes it (green), still refuses a stale Hermit
    /// package, and still judges extract-harness against the in-tree
    /// Reverie.
    #[test]
    fn check_tree_with_in_tree_reverie_judges_only_hermit_packages() {
        use std::fs;
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let repo = env::temp_dir().join(format!(
            "cargo-lock-cochange-in-tree-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&repo);
        let write = |rel: &str, text: &str| {
            let path = repo.join(rel);
            fs::create_dir_all(path.parent().expect("parent")).unwrap();
            fs::write(path, text).unwrap();
        };
        let detcore_model = |version: &str| {
            format!(
                "[package]\nname = \"detcore-model\"\nversion = \"{version}\"\n\
                 \n[dependencies]\nlibc = \"0.2.186\"\n\
                 reverie-memory = {{ version = \"0.5.7\", path = \"reverie/reverie-memory\" }}\n"
            )
        };
        let reverie_memory = |extra: &str| {
            format!(
                "[package]\nname = \"reverie-memory\"\nversion = \"0.5.7\"\n\
                 \n[dependencies]\nlibc = \"0.2.186\"\n{extra}"
            )
        };
        let pinned_memory_lock = "\
[[package]]\n\
name = \"reverie-memory\"\n\
version = \"0.5.7\"\n\
dependencies = [\n\
 \"libc\",\n\
 \"reverie-renamed-in-tree\",\n\
]\n";
        let members_lock = "\
[[package]]\n\
name = \"hermit-liteinst-runtime-artifact\"\n\
version = \"0.5.7\"\n\
\n\
[[package]]\n\
name = \"hermit-liteinst-runtime-build\"\n\
version = \"0.5.7\"\n\
\n";
        let libc_lock = "\
version = 4\n\
\n\
[[package]]\n\
name = \"libc\"\n\
version = \"0.2.186\"\n\
source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
\n";
        write("Cargo.toml", "[workspace]\nmembers = [\"detcore-model\"]\n");
        write("detcore-model/Cargo.toml", &detcore_model("0.5.7"));
        write(
            "reverie/Cargo.toml",
            "[workspace]\nmembers = [\"reverie-memory\"]\n",
        );
        // The in-tree Reverie is past the pin: it gained zerocopy.
        write(
            "reverie/reverie-memory/Cargo.toml",
            &reverie_memory("zerocopy = { version = \"0.8.63\", features = [\"derive\"] }\n"),
        );
        write(
            "liteinst-runtime-build/Cargo.toml",
            "[workspace]\nmembers = [\"runtime\"]\n\
             \n[package]\nname = \"hermit-liteinst-runtime-build\"\nversion = \"0.5.7\"\n",
        );
        write(
            "liteinst-runtime-build/runtime/Cargo.toml",
            "[package]\nname = \"hermit-liteinst-runtime-artifact\"\nversion = \"0.5.7\"\n",
        );
        write(
            "reverie/reverie-liteinst/extract-harness/Cargo.toml",
            "[package]\nname = \"s5b-extract-harness\"\nversion = \"0.0.0\"\n\
             \n[workspace]\n\n[dependencies]\nlibc = \"0.2.186\"\n",
        );
        // Hermit's nested lock records its two members, the pin's
        // reverie-memory (no zerocopy) and a package reverie-memory
        // depends on at the pin that the in-tree Reverie has since
        // renamed away.
        let liteinst_lock = format!(
            "{libc_lock}[[package]]\nname = \"detcore-model\"\nversion = \"0.5.7\"\n\
             dependencies = [\n \"libc\",\n \"reverie-memory\",\n]\n\
             \n{members_lock}{pinned_memory_lock}\n\
             [[package]]\nname = \"reverie-renamed-in-tree\"\nversion = \"0.5.7\"\n"
        );
        write("liteinst-runtime-build/Cargo.lock", &liteinst_lock);
        let extract_lock = format!(
            "{libc_lock}[[package]]\nname = \"s5b-extract-harness\"\nversion = \"0.0.0\"\n\
             dependencies = [\n \"libc\",\n]\n"
        );
        write(
            "reverie/reverie-liteinst/extract-harness/Cargo.lock",
            &extract_lock,
        );

        // Red half: a Hermit checkout judges the nested lock against
        // reverie/ and refuses both Reverie packages.
        assert_eq!(
            check_tree(&repo, ReverieSource::Pinned).expect("tree check runs"),
            1,
            "a Hermit checkout must refuse the nested lock against its reverie/"
        );

        // Green half: fbsource judges only Hermit's packages there.
        assert_eq!(
            check_tree(&repo, ReverieSource::InTree).expect("tree check runs"),
            0,
            "the in-tree Reverie must not judge Hermit's nested lock"
        );

        // Kept: a stale Hermit package still refuses in fbsource.
        write("detcore-model/Cargo.toml", &detcore_model("0.5.8"));
        assert_eq!(
            check_tree(&repo, ReverieSource::InTree).expect("tree check runs"),
            1,
            "a stale Hermit package must still refuse"
        );
        write("detcore-model/Cargo.toml", &detcore_model("0.5.7"));

        // Kept: a Hermit workspace member renamed without regenerating
        // the lock refuses in both trees. Its old name is depended on by
        // nothing, so it is not taken for a pinned Reverie package, and
        // its new name is not in the lock.
        let runtime = "liteinst-runtime-build/runtime/Cargo.toml";
        write(
            runtime,
            "[package]\nname = \"hermit-liteinst-runtime-renamed\"\nversion = \"0.5.7\"\n",
        );
        for source in [ReverieSource::InTree, ReverieSource::Pinned] {
            assert_eq!(
                check_tree(&repo, source).expect("tree check runs"),
                1,
                "a renamed workspace member must refuse ({source:?})"
            );
        }
        write(
            runtime,
            "[package]\nname = \"hermit-liteinst-runtime-artifact\"\nversion = \"0.5.7\"\n",
        );

        // Kept: a member added without regenerating the lock refuses in
        // both trees. Nothing in the lock names it, so only the member
        // rule sees it.
        let liteinst_root = "liteinst-runtime-build/Cargo.toml";
        write(
            liteinst_root,
            "[workspace]\nmembers = [\"runtime\", \"runtime2\"]\n\
             \n[package]\nname = \"hermit-liteinst-runtime-build\"\nversion = \"0.5.7\"\n",
        );
        write(
            "liteinst-runtime-build/runtime2/Cargo.toml",
            "[package]\nname = \"hermit-liteinst-runtime-added\"\nversion = \"0.5.7\"\n",
        );
        for source in [ReverieSource::InTree, ReverieSource::Pinned] {
            assert_eq!(
                check_tree(&repo, source).expect("tree check runs"),
                1,
                "a member missing from the lock must refuse ({source:?})"
            );
        }
        write(
            liteinst_root,
            "[workspace]\nmembers = [\"runtime\"]\n\
             \n[package]\nname = \"hermit-liteinst-runtime-build\"\nversion = \"0.5.7\"\n",
        );

        // A recorded path package that no manifest names and nothing
        // depends on is not the pin's either: it refuses in fbsource.
        write(
            "liteinst-runtime-build/Cargo.lock",
            &format!(
                "{liteinst_lock}\n[[package]]\nname = \"hermit-removed-crate\"\nversion = \"0.5.7\"\n"
            ),
        );
        assert_eq!(
            check_tree(&repo, ReverieSource::InTree).expect("tree check runs"),
            1,
            "an unreferenced package with no manifest must refuse"
        );
        write("liteinst-runtime-build/Cargo.lock", &liteinst_lock);
        assert_eq!(
            check_tree(&repo, ReverieSource::InTree).expect("tree check runs"),
            0,
            "the restored tree passes again"
        );

        // Kept: extract-harness is judged against the in-tree Reverie.
        write(
            "reverie/reverie-liteinst/extract-harness/Cargo.lock",
            &format!("{extract_lock}\n{pinned_memory_lock}"),
        );
        assert_eq!(
            check_tree(&repo, ReverieSource::InTree).expect("tree check runs"),
            1,
            "extract-harness must still be judged against the in-tree Reverie"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn lockfile_packages_reads_each_package_name_once() {
        let text = "\
version = 4\n\
\n\
[[package]]\n\
name = \"detcore-model\"\n\
version = \"0.5.7\"\n\
dependencies = [\n\
 \"reverie-procfs\",\n\
]\n\
\n\
[[package]]\n\
name = \"reverie-procfs\"\n\
version = \"0.5.7\"\n\
\n\
[metadata]\n\
name = \"not-a-package\"\n";
        assert_eq!(
            lockfile_packages(text),
            packages(&["detcore-model", "reverie-procfs"])
        );
    }

    /// The CI shape that must be able to fail: a CLEAN checkout whose HEAD
    /// commit touched a manifest without its lock, judged against HEAD^
    /// (the change's parent). Against HEAD itself the diff is empty and
    /// the lint could never fire — the seat-review defect this test pins.
    /// Real git, no network: two commits in a scratch repository.
    #[test]
    fn parent_base_refuses_committed_manifest_only_change_in_clean_tree() {
        use std::fs;
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let repo =
            env::temp_dir().join(format!("cargo-lock-cochange-{}-{seq}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);
        fs::create_dir_all(repo.join("detcore")).expect("fixture dir");
        fs::create_dir_all(repo.join("liteinst-runtime-build")).expect("fixture dir");
        fs::create_dir_all(repo.join("reverie/reverie-liteinst/extract-harness"))
            .expect("fixture dir");
        let git = |args: &[&str]| {
            let out = git_command()
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .expect("fixture git (git on PATH)");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "cochange test"]);
        git(&["config", "user.email", "cochange-test@example.invalid"]);
        git(&["config", "commit.gpgsign", "false"]);
        fs::write(repo.join("detcore/Cargo.toml"), "[package]\nname=\"d\"\n").unwrap();
        fs::write(repo.join("Cargo.lock"), "version = 4\n").unwrap();
        // The co-owner rule reads the nested resolution from this lock;
        // its one package is unrelated to the fixture manifest, so the
        // verdicts below stay the single-owner ones under test.
        fs::write(
            repo.join("liteinst-runtime-build/Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"unrelated-fixture-crate\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        fs::write(
            repo.join("reverie/reverie-liteinst/extract-harness/Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"unrelated-fixture-crate\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        git(&["add", "--all"]);
        git(&["commit", "-qm", "base"]);
        fs::write(repo.join("detcore/Cargo.toml"), "[package]\nname=\"d2\"\n").unwrap();
        git(&["add", "--all"]);
        git(&["commit", "-qm", "manifest only"]);

        let judge = |base: &str| {
            run_with_config(Config {
                repo: Some(repo.clone()),
                base: Some(base.to_string()),
                files_from_stdin: false,
                check_tree: false,
                reverie: ReverieSource::Pinned,
            })
            .expect("lint runs")
        };
        // Clean tree vs HEAD: nothing changed (the old, vacuous wiring).
        assert_eq!(judge("HEAD"), 0, "HEAD base sees no change in a clean tree");
        // Vs the change's parent: the committed omission refuses.
        assert_eq!(
            judge("HEAD^"),
            1,
            "parent base must refuse manifest-only commit"
        );
        // Including the lock (even uncommitted) satisfies the same base.
        fs::write(repo.join("Cargo.lock"), "version = 4\n# regenerated\n").unwrap();
        assert_eq!(judge("HEAD^"), 0, "lock included must pass at parent base");
        let _ = fs::remove_dir_all(&repo);
    }
}
