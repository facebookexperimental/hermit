#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Refuse the Buck graph drift that breaks a pristine checkout before any
//! Buck build runs: a shim signature gap, a stale or missing shared-cell
//! alias, and the one buildscript-cfg pin the generated graph depends on.
//!
//! A pristine `./bootstrap/buck2 build //hermit-cli:hermit` at e52db064
//! failed four ways, none of which any cheap check caught (the F5a fixes;
//! see `docs/BUCK2_OSS.md` for the build this protects):
//!
//! - B1: `hermit-cli/BUCK` passes `test_srcs` to `rust_library`, but the
//!   shim's `rust_library` in `shim/shims.bzl` did not declare the
//!   parameter its `rust_binary` already swallowed, so it reached the
//!   prelude rule, which rejects it at analysis.
//! - B2: `shim/third-party/rust/shared-cell-aliases.txt` still named
//!   `liteinst2` after the crate was renamed `liteinst`, so
//!   `bootstrap/regenerate-rust-deps` failed -- but only after vendoring
//!   every crate.
//! - B3: BUCK files name third-party crates without a version suffix
//!   (`fbsource//third-party/rust:linux-raw-sys`). Reindeer emits such an
//!   alias only for the manifest's own direct dependencies, and the
//!   shared-cell list appends the rest; `linux-raw-sys` and `quickcheck`
//!   were in neither, so the targets did not exist.
//! - B4: `generic-array`'s build script emits `--cfg=relaxed_coherence`
//!   on every supported rustc, which is what implements
//!   `From<GenericArray> for [T; n]` behind detcore's
//!   `<[u8; 32]>::from(Sha256::digest(..))`. Its fixups disabled the
//!   build script without replicating the cfg, so Buck alone failed
//!   with E0277 where Cargo compiled.
//!
//! This check is static: no Buck, no network, no generated files. A
//! reference counts as covered when the shared-cell list names it or
//! when it is a direct (non-dev, non-build) dependency of a workspace
//! member, which is the rule Reindeer aliases by
//! (`shim/third-party/rust/shared-cell-aliases.txt` documents it). That
//! reproduces the generated graph's verdict for B1-B3 in under a
//! second; `bootstrap/regenerate-rust-deps` stays the authority on the
//! graph itself and still refuses an alias with no versioned target.
//! References are read from this checkout's tracked BUCK, PACKAGE and
//! .bzl files and from the Reverie submodule's, because one third-party
//! cell serves both. An uninitialized submodule is a no-result (exit
//! 75), never a pass.
//!
//! A reference counts by the name the shim resolves it to: the shim
//! renames a few internal crate names (`sha2-010`) in the exact rewrite
//! rules of `shim/build_defs/lib/oss.bzl`, which this check reads. Labels
//! inside an `autocargo = {...}` argument are not references: that dict
//! describes the Cargo manifest autocargo generates internally, the shim
//! swallows it, and Buck never resolves the labels in it.
//!
//! Four referenced crates have no generated target and are named here
//! instead of being silently skipped: `io-uring`, `num_cpus`,
//! `quickcheck_macros` and `test-case` are test-only dependencies outside
//! what Reindeer resolves. Giving them targets, trimming the test targets
//! that name them, or another resolution is an open decision; until it is taken
//! they are the only tolerated gaps, and the check fails if one stops
//! being referenced or gains coverage, so the exception cannot rot.
//! Three more names, `prost`, `tonic` and `tonic-prost`, occur only
//! inside the shim's never-called `rust_protobuf_library`; they are
//! tolerated only while confined to `shim/shims.bzl`.
//!
//! What this cannot reach, stated plainly: the general B4 class. A
//! crate whose disabled build script emitted a cfg its consumers need
//! is invisible here without vendored sources (which are generated,
//! hence absent from a fresh checkout); only the generic-array pin is
//! checked. Compiling the consumers -- the pristine Buck build -- is
//! the check for the class. Likewise this does not prove the shim
//! forwards nothing else the prelude rejects; it pins the swallowed
//! test-parameter parity between `rust_library` and `rust_binary`
//! that B1 broke.
//!
//! Run with no argument inside the checkout; `--help` or `-h` prints
//! the usage. Exit status: 0 when no drift is found, 1 on drift or a
//! file this check refuses to read, 2 on a usage or load error, 75
//! when the Reverie submodule is not initialized.
//!
//! ```cargo
//! [dependencies]
//! toml = "0.8"
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

/// The prefix of each line this check prints.
const NAME: &str = "check-buck-graph-drift.rs";
const USAGE: &str = "usage: check-buck-graph-drift.rs [-h | --help]";
const HELP: &str = "Refuse Buck graph drift (shim test-parameter gap, stale or missing \
     shared-cell alias, missing generic-array buildscript cfg) in this checkout. Run it \
     with no argument from inside a Hermit checkout. Exit status: 0 clean, 1 drift, \
     2 usage or load error, 75 when the Reverie submodule is not initialized.";
/// How BUCK files spell an unsuffixed third-party Rust target.
const REF_PREFIX: &str = "fbsource//third-party/rust:";
/// The Meta-only test parameters both shim entry points must swallow.
const SWALLOWED_TEST_PARAMS: [&str; 3] = ["test_deps", "test_env", "test_srcs"];
/// Referenced crates with no Reindeer target; resolution undecided.
/// `io-uring` is a dependency of Reverie's KVM unit tests
/// (reverie-kvm-unittest).
const KNOWN_MISSING: [&str; 4] = ["io-uring", "num_cpus", "quickcheck_macros", "test-case"];
/// Names only the shim's dead `rust_protobuf_library` may mention.
const DEAD_REFS: [&str; 3] = ["prost", "tonic", "tonic-prost"];
const DEAD_REF_FILES: [&str; 1] = ["shim/shims.bzl"];
const SHIMS: &str = "shim/shims.bzl";
/// The shim's label rewrite rules, whose exact entries rename crates.
const REWRITES: &str = "shim/build_defs/lib/oss.bzl";
const ALIASES: &str = "shim/third-party/rust/shared-cell-aliases.txt";
const GENERIC_ARRAY_FIXUPS: &str = "shim/third-party/rust/fixups/generic-array/fixups.toml";
/// The cfg generic-array's build script emits on rustc >= 1.41.0.
const RELAXED_COHERENCE: &str = "relaxed_coherence";

/// Why a run produced no verdict (exit 75) rather than a pass or a failure.
struct NoResult(String);

fn main() -> ExitCode {
    rust_script_prelude::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{USAGE}\n{HELP}");
        return ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        eprintln!("{NAME}: {USAGE}");
        return ExitCode::from(2);
    }
    match run() {
        Ok(summary) => {
            println!("{NAME}: {summary}");
            ExitCode::SUCCESS
        }
        Err(Outcome::Drift(violations)) => {
            for violation in &violations {
                eprintln!("{NAME}: {violation}");
            }
            ExitCode::from(1)
        }
        Err(Outcome::Load(message)) => {
            eprintln!("{NAME}: {message}");
            ExitCode::from(2)
        }
        Err(Outcome::Undecided(NoResult(message))) => {
            println!("NO-RESULT-CASE: {NAME}: {message}");
            ExitCode::from(75)
        }
    }
}

enum Outcome {
    Drift(Vec<String>),
    Load(String),
    Undecided(NoResult),
}

impl From<String> for Outcome {
    fn from(message: String) -> Self {
        Outcome::Load(message)
    }
}

impl From<NoResult> for Outcome {
    fn from(no_result: NoResult) -> Self {
        Outcome::Undecided(no_result)
    }
}

fn run() -> Result<String, Outcome> {
    let root = repo_root()?;
    let mut violations: Vec<String> = Vec::new();

    // B1: both shim entry points swallow the same test parameters.
    let shims = read(&root, SHIMS)?;
    for def in ["rust_library", "rust_binary"] {
        let Some(params) = shim_params(&shims, def) else {
            return Err(format!("{SHIMS}: no `def {def}(` found; update this check").into());
        };
        for param in SWALLOWED_TEST_PARAMS {
            if !params.contains(param) {
                violations.push(format!(
                    "{SHIMS}: `def {def}` does not declare `{param}`, so a BUCK file \
                     passing it forwards it to the prelude rule, which rejects it; \
                     declare it and add it to `_unused` as the sibling entry point does"
                ));
            }
        }
    }

    // The alias list: sorted, unique, and every entry a locked crate (B2).
    let aliases = read(&root, ALIASES)?;
    let shared: Vec<String> = aliases
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect();
    let mut sorted = shared.clone();
    sorted.sort();
    if sorted != shared {
        violations.push(format!("{ALIASES}: entries are not sorted"));
    }
    let shared_set: BTreeSet<&str> = shared.iter().map(String::as_str).collect();
    if shared_set.len() != shared.len() {
        violations.push(format!("{ALIASES}: duplicate entries"));
    }
    let locked = lock_packages(&read(&root, "Cargo.lock")?)?;
    for entry in &shared {
        if !locked.contains(entry.as_str()) {
            violations.push(format!(
                "{ALIASES}: `{entry}` is not a package in Cargo.lock (renamed or \
                 removed crate); regeneration would fail on it after vendoring"
            ));
        }
    }

    // References from both cells, and what covers them (B3).
    let rewrites = read(&root, REWRITES)?;
    let renames = shim_renames(&rewrites);
    let label_renames = first_party_renames(&rewrites);
    let mut ref_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut label_files: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut files_scanned = 0usize;
    for file in tracked_build_files(&root)?
        .into_iter()
        .chain(reverie_build_files(&root)?)
    {
        files_scanned += 1;
        let text = without_autocargo(&read(&root, &file)?);
        for name in extract_refs(&text) {
            ref_files.entry(name).or_default().insert(file.clone());
        }
        for label in label_renames.keys() {
            if references_label(&text, label) {
                label_files.entry(label).or_default().insert(file.clone());
            }
        }
    }
    let direct = direct_deps(&root)?;
    let known: BTreeSet<&str> = KNOWN_MISSING.into_iter().collect();
    let dead: BTreeSet<&str> = DEAD_REFS.into_iter().collect();
    let mut via_shared = 0usize;
    let mut via_direct = 0usize;
    for (name, files) in &ref_files {
        let shown = files.iter().next().map(String::as_str).unwrap_or("?");
        // The target the shim resolves the reference to.
        let resolved = renames.get(name).unwrap_or(name);
        if shared_set.contains(resolved.as_str()) {
            via_shared += 1;
        } else if direct.contains(resolved) {
            via_direct += 1;
        } else if known.contains(name.as_str()) {
            // Tolerated gap, named in the summary.
        } else if dead.contains(name.as_str()) {
            let outside: Vec<&str> = files
                .iter()
                .map(String::as_str)
                .filter(|file| !DEAD_REF_FILES.contains(file))
                .collect();
            if !outside.is_empty() {
                violations.push(format!(
                    "`{name}` is tolerated only inside the shim's dead \
                     rust_protobuf_library ({}), but {} references it; give it a \
                     real target or remove the reference",
                    DEAD_REF_FILES.join(", "),
                    outside.join(", ")
                ));
            }
        } else {
            violations.push(format!(
                "{shown}: `{REF_PREFIX}{name}` has no target: it is neither in \
                 {ALIASES} nor a direct dependency of a workspace member, so \
                 neither Reindeer nor the shared-cell append emits it"
            ));
        }
        if (known.contains(name.as_str()) || dead.contains(name.as_str()))
            && (shared_set.contains(name.as_str()) || direct.contains(name))
        {
            violations.push(format!(
                "`{name}` is listed as an exception in this check but is now \
                 covered; remove it from the exception list"
            ));
        }
    }
    for name in known.iter().chain(dead.iter()) {
        if !ref_files.contains_key(*name) {
            violations.push(format!(
                "`{name}` is listed as an exception in this check but nothing \
                 references it any more; remove it from the exception list"
            ));
        }
    }
    // A first-party label the shim renames to a crate counts as a reference
    // to that crate, and needs the crate's target as any reference does.
    for (label, krate) in &label_renames {
        match label_files.get(label.as_str()) {
            None => violations.push(format!(
                "{REWRITES}: the rule renaming `{label}` to `third-party/rust:{krate}` \
                 matches no reference in any BUCK file; remove the stale rule"
            )),
            Some(files) => {
                let shown = files.iter().next().map(String::as_str).unwrap_or("?");
                if shared_set.contains(krate.as_str()) {
                    via_shared += 1;
                } else if direct.contains(krate) {
                    via_direct += 1;
                } else {
                    violations.push(format!(
                        "{shown}: `{label}`, which {REWRITES} renames to \
                         `third-party/rust:{krate}`, has no target: `{krate}` is \
                         neither in {ALIASES} nor a direct dependency of a \
                         workspace member"
                    ));
                }
            }
        }
    }
    for entry in &shared {
        if !ref_files.contains_key(entry) && !label_renames.values().any(|krate| krate == entry) {
            violations.push(format!(
                "{ALIASES}: `{entry}` is referenced by no BUCK file in either \
                 cell; remove the stale alias"
            ));
        }
    }

    // B4: the generic-array buildscript cfg must survive in some form.
    let fixups: toml::Value = read(&root, GENERIC_ARRAY_FIXUPS)?
        .parse()
        .map_err(|error| format!("{GENERIC_ARRAY_FIXUPS}: {error}"))?;
    if !buildscript_cfg_preserved(&fixups) {
        violations.push(format!(
            "{GENERIC_ARRAY_FIXUPS}: neither runs generic-array's build script \
             nor declares its `{RELAXED_COHERENCE}` cfg, so the generated graph \
             loses `From<GenericArray> for [T; n]` and detcore fails under \
             Buck with E0277 where Cargo compiles"
        ));
    }

    if violations.is_empty() {
        Ok(format!(
            "{} unsuffixed third-party references and {} first-party label(s) the \
             shim renames to a crate ({}) in {files_scanned} build files: \
             {via_shared} via the shared-cell list ({} entries, all locked crates), \
             {via_direct} via Reindeer's direct-dependency aliases, {} known-missing \
             tolerated ({}), {} dead shim-only tolerated ({}); shim test-parameter \
             parity holds; generic-array preserves `{RELAXED_COHERENCE}`",
            ref_files.len(),
            label_renames.len(),
            label_renames
                .values()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            shared.len(),
            KNOWN_MISSING.len(),
            KNOWN_MISSING.join(", "),
            DEAD_REFS.len(),
            DEAD_REFS.join(", "),
        ))
    } else {
        Err(Outcome::Drift(violations))
    }
}

/// The checkout root, refusing anything that is not a Hermit checkout.
fn repo_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("git could not run: {error}"))?;
    if !output.status.success() {
        return Err("run this inside a Hermit checkout, outside its submodules".into());
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !root.join(SHIMS).is_file() || !root.join("Cargo.lock").is_file() {
        return Err("run this inside a Hermit checkout, outside its submodules".into());
    }
    Ok(root)
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

/// This checkout's tracked Starlark build files, repo-relative.
fn tracked_build_files(root: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "ls-files",
            "--",
            ":(glob)**/BUCK",
            ":(glob)**/PACKAGE",
            ":(glob)**/*.bzl",
        ])
        .output()
        .map_err(|error| format!("git ls-files could not run: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// The Reverie submodule's build files, repo-relative. A missing
/// `reverie/BUCK` means the submodule is not initialized: no verdict.
fn reverie_build_files(root: &Path) -> Result<Vec<String>, Outcome> {
    if !root.join("reverie/BUCK").is_file() {
        return Err(NoResult(
            "reverie/BUCK is absent, so the Reverie submodule is not initialized \
             and its third-party references cannot be checked; run \
             `git submodule update --init reverie`"
                .into(),
        )
        .into());
    }
    let mut files = Vec::new();
    let mut stack = vec![root.join("reverie")];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !name.starts_with('.') && name.as_ref() != "buck-out" {
                    stack.push(path);
                }
            } else if name.as_ref() == "BUCK" || name.ends_with(".bzl") {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| format!("{} is outside the checkout", path.display()))?;
                files.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Every `fbsource//third-party/rust:<name>` target named in `text`.
/// `text` with the value of every `autocargo = {...}` argument removed.
/// A dict that does not close is kept whole, so that what this cannot
/// parse still counts.
fn without_autocargo(text: &str) -> String {
    const KEY: &str = "autocargo = {";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(KEY) {
        let open = at + KEY.len() - 1;
        let mut depth = 0i32;
        let mut close = None;
        for (offset, byte) in rest[open..].bytes().enumerate() {
            match byte {
                b'{' | b'[' | b'(' => depth += 1,
                b'}' | b']' | b')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + offset);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str("{}");
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The crate renames among the shim's exact rewrite rules: each
/// `"third-party/rust:<from>": "third-party/rust:<to>",` line.
fn shim_renames(text: &str) -> BTreeMap<String, String> {
    const CRATE: &str = "\"third-party/rust:";
    let mut renames = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix(CRATE) else {
            continue;
        };
        let Some((from, rest)) = rest.split_once("\": ") else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(CRATE) else {
            continue;
        };
        let Some(to) = rest.strip_suffix("\",") else {
            continue;
        };
        renames.insert(from.to_string(), to.to_string());
    }
    renames
}

/// The shim's exact rules that rename a first-party internal label to a crate,
/// `"hermetic_infra/<package>:<name>": "third-party/rust:<crate>",`: fbsource
/// builds that code under its own tree (fb-only/), where this tree takes the
/// crate. Keyed by the label as BUCK files spell it, `//hermetic_infra/...`.
fn first_party_renames(text: &str) -> BTreeMap<String, String> {
    const FIRST_PARTY: &str = "\"hermetic_infra/";
    const CRATE: &str = "\"third-party/rust:";
    let mut renames = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix(FIRST_PARTY) else {
            continue;
        };
        let Some((from, rest)) = rest.split_once("\": ") else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(CRATE) else {
            continue;
        };
        let Some(to) = rest.strip_suffix("\",") else {
            continue;
        };
        renames.insert(format!("//hermetic_infra/{from}"), to.to_string());
    }
    renames
}

/// Whether build-file `text` names `label` (`//hermetic_infra/...`) as a
/// quoted label, cell-relative or in the fbcode cell.
fn references_label(text: &str, label: &str) -> bool {
    text.contains(&format!("\"{label}\"")) || text.contains(&format!("\"fbcode{label}\""))
}

fn extract_refs(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(REF_PREFIX) {
        let after = &rest[at + REF_PREFIX.len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !name.is_empty() {
            names.push(name);
        }
        rest = after;
    }
    names
}

/// The parameter names of `def <name>(...)` in Starlark source, or
/// `None` when there is no such definition. Defaults are dropped;
/// `*args`/`**kwargs` are kept verbatim and never match a real name.
fn shim_params(text: &str, def: &str) -> Option<BTreeSet<String>> {
    let needle = format!("def {def}(");
    let start = text.find(&needle)? + needle.len();
    let mut depth = 1i32;
    let mut end = None;
    for (offset, byte) in text[start..].bytes().enumerate() {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(start + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let list = &text[start..end?];
    let mut params = BTreeSet::new();
    let mut depth = 0i32;
    let mut part = String::new();
    let mut parts = Vec::new();
    for ch in list.chars() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut part));
                continue;
            }
            _ => {}
        }
        part.push(ch);
    }
    parts.push(part);
    for part in parts {
        let name = part.split('=').next().unwrap_or("").trim();
        if !name.is_empty() && name != "*" {
            params.insert(name.to_string());
        }
    }
    Some(params)
}

/// Package names recorded in Cargo.lock.
fn lock_packages(text: &str) -> Result<BTreeSet<String>, String> {
    let lock: toml::Value = text
        .parse()
        .map_err(|error| format!("Cargo.lock: {error}"))?;
    let mut names = BTreeSet::new();
    if let Some(packages) = lock.get("package").and_then(toml::Value::as_array) {
        for package in packages {
            if let Some(name) = package.get("name").and_then(toml::Value::as_str) {
                names.insert(name.to_string());
            }
        }
    }
    if names.is_empty() {
        return Err("Cargo.lock: no [[package]] entries read".into());
    }
    Ok(names)
}

/// Direct non-dev, non-build dependency package names of every
/// workspace member: the set Reindeer emits unsuffixed aliases for.
fn direct_deps(root: &Path) -> Result<BTreeSet<String>, String> {
    let manifest: toml::Value = read(root, "Cargo.toml")?
        .parse()
        .map_err(|error| format!("Cargo.toml: {error}"))?;
    let workspace = manifest
        .get("workspace")
        .ok_or("Cargo.toml: no [workspace] table")?;
    let members = workspace
        .get("members")
        .and_then(toml::Value::as_array)
        .ok_or("Cargo.toml: no workspace.members list")?;
    let inherited = workspace.get("dependencies");
    let mut direct = BTreeSet::new();
    for member in members {
        let member = member
            .as_str()
            .ok_or("Cargo.toml: workspace.members holds a non-string")?;
        if member.contains('*') {
            return Err(format!(
                "Cargo.toml: workspace member `{member}` is a glob, which this \
                 check does not expand; list members explicitly or teach it globs"
            ));
        }
        let path = format!("{member}/Cargo.toml");
        let member_manifest: toml::Value = read(root, &path)?
            .parse()
            .map_err(|error| format!("{path}: {error}"))?;
        collect_deps(&member_manifest, inherited, &path, &mut direct)?;
    }
    Ok(direct)
}

fn collect_deps(
    manifest: &toml::Value,
    inherited: Option<&toml::Value>,
    path: &str,
    direct: &mut BTreeSet<String>,
) -> Result<(), String> {
    let mut tables: Vec<&toml::Value> = Vec::new();
    if let Some(deps) = manifest.get("dependencies") {
        tables.push(deps);
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            if let Some(deps) = target.get("dependencies") {
                tables.push(deps);
            }
        }
    }
    for table in tables {
        let table = table
            .as_table()
            .ok_or_else(|| format!("{path}: a dependencies value is not a table"))?;
        for (key, value) in table {
            direct.insert(package_name(key, value, inherited, path)?);
        }
    }
    Ok(())
}

/// The crate a dependency entry names, through `package =` renames
/// and `workspace = true` inheritance.
fn package_name(
    key: &str,
    value: &toml::Value,
    inherited: Option<&toml::Value>,
    path: &str,
) -> Result<String, String> {
    let Some(table) = value.as_table() else {
        return Ok(key.to_string());
    };
    if table.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
        let entry = inherited.and_then(|deps| deps.get(key)).ok_or_else(|| {
            format!(
                "{path}: `{key}` inherits from the workspace but the root \
                         manifest has no such workspace dependency"
            )
        })?;
        return Ok(entry
            .get("package")
            .and_then(toml::Value::as_str)
            .unwrap_or(key)
            .to_string());
    }
    Ok(table
        .get("package")
        .and_then(toml::Value::as_str)
        .unwrap_or(key)
        .to_string())
}

/// Whether generic-array's fixups keep its build-script cfg: either
/// the build script runs, or some `cfgs` list declares the cfg.
fn buildscript_cfg_preserved(fixups: &toml::Value) -> bool {
    if fixups
        .get("buildscript")
        .and_then(|buildscript| buildscript.get("run"))
        .and_then(toml::Value::as_bool)
        == Some(true)
    {
        return true;
    }
    fn declares(value: &toml::Value) -> bool {
        match value {
            toml::Value::Table(table) => {
                table.get("cfgs").is_some_and(|cfgs| {
                    cfgs.as_array().is_some_and(|cfgs| {
                        cfgs.iter()
                            .any(|cfg| cfg.as_str() == Some(RELAXED_COHERENCE))
                    })
                }) || table.values().any(declares)
            }
            toml::Value::Array(items) => items.iter().any(declares),
            _ => false,
        }
    }
    declares(fixups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_are_extracted_verbatim() {
        let text = "\"fbsource//third-party/rust:linux-raw-sys\", \
                    \"fbsource//third-party/rust:num_cpus\", \
                    \"fbsource//third-party/rust:serde_json\", no ref here";
        assert_eq!(
            extract_refs(text),
            vec!["linux-raw-sys", "num_cpus", "serde_json"]
        );
        assert!(extract_refs("third-party/rust without the cell").is_empty());
    }

    #[test]
    fn autocargo_labels_are_not_references() {
        let text = "rust_library(\n    autocargo = {\"extra\": [(\"cc\", \
                    \"fbsource//third-party/rust:cc\")], \"x\": {}},\n    \
                    deps = [\"fbsource//third-party/rust:libc\"],\n)\n";
        assert_eq!(extract_refs(&without_autocargo(text)), vec!["libc"]);
        // A dict that does not close is kept, so its labels still count.
        let open = "autocargo = {\"fbsource//third-party/rust:cc\",";
        assert_eq!(extract_refs(&without_autocargo(open)), vec!["cc"]);
        let plain = "deps = [\"fbsource//third-party/rust:libc\"]";
        assert_eq!(without_autocargo(plain), plain);
    }

    #[test]
    fn shim_renames_read_exact_crate_rules() {
        let text = "    \"fbcode\": struct(\n        exact = {\n            \
                    \"common/rust/shed/fbinit:fbinit\": \"third-party/rust:fbinit\",\n        \
                    },\n    \"fbsource\": struct(\n        exact = {\n            \
                    \"third-party/rust:sha2-010\": \"third-party/rust:sha2\",\n        },\n";
        let renames = shim_renames(text);
        assert_eq!(renames.len(), 1);
        assert_eq!(renames.get("sha2-010").map(String::as_str), Some("sha2"));
    }

    #[test]
    fn first_party_renames_read_only_project_labels_renamed_to_crates() {
        let text = "    \"fbcode\": struct(\n        exact = {\n            \
                    \"common/rust/shed/fbinit:fbinit\": \"third-party/rust:fbinit\",\n            \
                    \"hermetic_infra/hermit/fb-only/dagrun:dagrun\": \"third-party/rust:dagrun\",\n            \
                    \"hermetic_infra/hermit/x:y\": \"third-party/ocaml:y\",\n        },\n    \
                    \"fbsource\": struct(\n        exact = {\n            \
                    \"third-party/rust:sha2-010\": \"third-party/rust:sha2\",\n        },\n";
        let renames = first_party_renames(text);
        assert_eq!(renames.len(), 1, "{renames:?}");
        assert_eq!(
            renames
                .get("//hermetic_infra/hermit/fb-only/dagrun:dagrun")
                .map(String::as_str),
            Some("dagrun")
        );
    }

    #[test]
    fn references_label_matches_quoted_labels_only() {
        let label = "//hermetic_infra/hermit/fb-only/dagrun:dagrun";
        assert!(references_label(
            "deps = [\"//hermetic_infra/hermit/fb-only/dagrun:dagrun\"]",
            label
        ));
        assert!(references_label(
            "\"fbcode//hermetic_infra/hermit/fb-only/dagrun:dagrun\",",
            label
        ));
        assert!(!references_label(
            "\"//hermetic_infra/hermit/fb-only/dagrun:dagrun-extra\"",
            label
        ));
        assert!(!references_label(
            "# //hermetic_infra/hermit/fb-only/dagrun:dagrun",
            label
        ));
    }

    #[test]
    fn shim_params_drop_defaults_and_nested_commas() {
        let text = "def rust_library(\n    name,\n    mapped_srcs = {},\n    \
                    test_srcs = None,\n    visibility = [\"PUBLIC\"],\n    **kwargs,\n):\n    pass\n";
        let params = shim_params(text, "rust_library").unwrap();
        assert!(params.contains("test_srcs"), "{params:?}");
        assert!(params.contains("name"), "{params:?}");
        assert!(params.contains("mapped_srcs"), "{params:?}");
        let missing = shim_params(text, "rust_binary");
        assert!(missing.is_none());
        let pre_fix = "def rust_library(name, test_deps = None, **kwargs):\n";
        assert!(
            !shim_params(pre_fix, "rust_library")
                .unwrap()
                .contains("test_srcs")
        );
    }

    #[test]
    fn package_names_follow_renames_and_inheritance() {
        let entry = |text: &str| {
            text.parse::<toml::Value>()
                .unwrap()
                .get("dep")
                .unwrap()
                .clone()
        };
        let plain = entry("dep = \"1\"");
        assert_eq!(package_name("serde", &plain, None, "x").unwrap(), "serde");
        let renamed = entry("dep = { package = \"real-crate\", version = \"1\" }");
        assert_eq!(
            package_name("alias", &renamed, None, "x").unwrap(),
            "real-crate"
        );
        let inherited: toml::Value = "inherited-dep = { package = \"ws-crate\" }"
            .parse()
            .unwrap();
        let via_ws = entry("dep = { workspace = true }");
        assert_eq!(
            package_name("inherited-dep", &via_ws, Some(&inherited), "x").unwrap(),
            "ws-crate"
        );
        assert!(package_name("inherited-dep", &via_ws, None, "x").is_err());
    }

    #[test]
    fn buildscript_cfg_needs_the_script_or_the_cfg() {
        let parses = |text: &str| text.parse::<toml::Value>().unwrap();
        assert!(!buildscript_cfg_preserved(&parses(
            "buildscript.run = false"
        )));
        assert!(buildscript_cfg_preserved(&parses("buildscript.run = true")));
        assert!(buildscript_cfg_preserved(&parses(
            "buildscript.run = false\ncfgs = [\"relaxed_coherence\"]"
        )));
        assert!(!buildscript_cfg_preserved(&parses(
            "buildscript.run = false\ncfgs = [\"something_else\"]"
        )));
        assert!(buildscript_cfg_preserved(&parses(
            "['cfg(target_os = \"linux\")']\ncfgs = [\"relaxed_coherence\"]"
        )));
    }

    #[test]
    fn lock_packages_reads_every_entry() {
        let lock = "[[package]]\nname = \"liteinst\"\nversion = \"0.2.0\"\n\
                    [[package]]\nname = \"sha2\"\nversion = \"0.10.9\"\n";
        let names = lock_packages(lock).unwrap();
        assert!(names.contains("liteinst"));
        assert!(names.contains("sha2"));
        assert!(!names.contains("liteinst2"));
    }
}
