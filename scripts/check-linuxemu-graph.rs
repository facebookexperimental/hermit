#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! ```cargo
//! [dependencies]
//! serde_json = "1"
//! toml = "0.8"
//! ```
//!
//! Refuse LinuxEmu extension packages in Hermit's build graph.
//!
//! Hermit builds only the LinuxEmu core (`reverie-linuxemu`). Optional
//! standalone extensions (`reverie-linuxemu-ext-*`, umbrella
//! `reverie-linuxemu-full`) are separate crates selected only by
//! standalone composition roots; with no extension crate in the graph
//! the code does not exist in the Hermit build (design:
//! ai_docs/plans/2026-11-05-linuxemu-core-optional-extensions.md;
//! restart: 2026-11-15-linuxemu-modularity-rebaseline.md, Slice 1).
//! This gate is what makes that a property of the queue rather than of
//! reviewer attention. It also refuses any non-default feature being
//! enabled on `reverie-linuxemu` (Cargo features unify across a graph,
//! so a feature enabled anywhere compiles into every build).
//!
//! Tracked-file scope comes from the repository's own VCS, detected
//! at the audit root: `git ls-files` under git (audit root must be the
//! git top-level), `sl files` under Sapling (the fbsource main
//! environment; audit root may be a subtree such as
//! `fbcode/hermetic_infra/hermit`, found by `.sl`/`.hg`/`.eden` marker walk
//! so discovery can never escape an Eden mount into an outer repo).
//! Anything else is CHECKER ERROR.
//!
//! Evidence comes from real parsers only, never line or text scans of
//! manifests (a `features` list on its own line or a renamed
//! `package = "..."` key spread over lines must not be able to slip a
//! text scan — Slice-1 seat finding, 2026-11-15):
//!   * every tracked `Cargo.lock`, parsed as TOML: the all-platforms
//!     package census; any denied package present refuses;
//!   * `cargo metadata --format-version 1 --locked --offline` per
//!     lockfile workspace: resolved package identities (renames already
//!     resolved by Cargo) and the enabled feature set of every
//!     `reverie-linuxemu` node, judged against its default closure.
//! Tracked `BUCK` files get dependency-edge extraction in this slice;
//! the real `buck2 query` leg is Slice 1b (same train). The interim
//! extraction does NOT scan: (1) arbitrary-named variables referenced
//! by `deps` (no symbol resolution) or `.append`/`.extend` mutation;
//! (2) labels built by string concatenation/format or by list
//! comprehension over names; (3) macro positional arguments,
//! non-dep-named macro kwargs, or macro-internal deps in `.bzl`
//! files (`.bzl` is out of scope entirely); (4) a bracket-close
//! followed by a newline continuation (`+ [...]` on the next line).
//! Known missed real edges, excluded until Slice 1b:
//! `hermit-cli/integration_tests.bzl:137-144` (macro forwards
//! `spec["deps"]`) and `hermit-cli/integration_tests_data.bzl:99-100`
//! (`reverie-ptrace` data; both point at ALLOWED targets today).
//!
//! The deny-list is `scripts/linuxemu-graph-deny.txt`. A denied package
//! may be carried only as flagged, expiring debt (`allow-until`): the
//! audit names it on every green run and refuses once the date passes.
//! Today that is `reverie-procfs` (owner 6463: not ratified as core;
//! removed by Slice 2, which moves it behind the extension seam).
//!
//! THREE OUTCOMES (house rule; never collapse the third):
//!   0 PASS (flagged allow-until debt may still be printed),
//!   1 REFUSE (a denied package or non-default feature is in the graph),
//!   2 CHECKER ERROR (could not determine: git, cargo, deny-list, or a
//!     failed negative control). This gate fails closed.
//!
//! Before trusting a PASS the checker runs negative controls: scratch
//! workspaces with a planted extension package, a planted feature on
//! its own line, a multiline renamed key, and an expired allow must
//! each be refused; a clean workspace must pass under BOTH git and
//! Sapling scopes, an Eden-style Sapling checkout nested inside an
//! outer git repository must resolve to the inner Sapling root even
//! though bare `sl root` escapes to the outer repository, and a
//! directory under no VCS must error. Sapling controls are counted,
//! named skips only when `sl` itself is absent; git controls always run.
//! `--skip-negative-control` exists for the twin test only.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const RC_PASS: i32 = 0;
const RC_REFUSE: i32 = 1;
const RC_ERROR: i32 = 2;
const CORE_CRATE: &str = "reverie-linuxemu";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Date {
    y: i64,
    m: u32,
    d: u32,
}

impl Date {
    fn parse(s: &str) -> Result<Date, String> {
        let mut it = s.split('-');
        let (y, m, d) = match (it.next(), it.next(), it.next(), it.next()) {
            (Some(y), Some(m), Some(d), None) => (y, m, d),
            _ => return Err(format!("bad date {s:?} (want YYYY-MM-DD)")),
        };
        let y: i64 = y.parse().map_err(|_| format!("bad date {s:?}"))?;
        let m: u32 = m.parse().map_err(|_| format!("bad date {s:?}"))?;
        let d: u32 = d.parse().map_err(|_| format!("bad date {s:?}"))?;
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return Err(format!("bad date {s:?}"));
        }
        Ok(Date { y, m, d })
    }

    fn today() -> Result<Date, String> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("system clock before epoch: {e}"))?
            .as_secs() as i64;
        // Civil-from-days (Howard Hinnant's algorithm), UTC.
        let z = secs.div_euclid(86_400) + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        Ok(Date {
            y: if m <= 2 { y + 1 } else { y },
            m,
            d,
        })
    }
}

#[derive(Debug, Default)]
struct DenyList {
    /// (glob, is_prefix) — a glob is an exact name or `prefix*`.
    deny: Vec<(String, bool)>,
    allow_until: BTreeMap<String, (Date, String)>,
}

impl DenyList {
    fn load(path: &Path) -> Result<DenyList, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read deny-list {}: {e}", path.display()))?;
        let mut list = DenyList::default();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let at = format!("{}:{}", path.display(), n + 1);
            if let Some(rest) = line.strip_prefix("deny ") {
                let glob = rest.trim();
                if glob.is_empty() || glob.split_whitespace().count() != 1 {
                    return Err(format!("{at}: malformed deny line {raw:?}"));
                }
                if let Some(prefix) = glob.strip_suffix('*') {
                    if prefix.contains('*') {
                        return Err(format!("{at}: only one trailing * allowed"));
                    }
                    list.deny.push((prefix.to_string(), true));
                } else {
                    list.deny.push((glob.to_string(), false));
                }
            } else if let Some(rest) = line.strip_prefix("allow-until ") {
                let mut parts = rest.splitn(3, char::is_whitespace);
                let name = parts.next().unwrap_or("");
                let date = parts.next().unwrap_or("");
                let reason = parts.next().unwrap_or("").trim();
                if name.is_empty() || date.is_empty() || reason.is_empty() {
                    return Err(format!("{at}: allow-until needs <name> <date> <reason>"));
                }
                list.allow_until
                    .insert(name.to_string(), (Date::parse(date)?, reason.to_string()));
            } else {
                return Err(format!(
                    "{at}: unknown directive (deny-list is strict): {raw:?}"
                ));
            }
        }
        if list.deny.is_empty() {
            return Err("deny-list names nothing to deny; refusing to trust it".to_string());
        }
        Ok(list)
    }

    fn denies(&self, name: &str) -> bool {
        self.deny.iter().any(|(g, prefix)| {
            if *prefix {
                name.starts_with(g.as_str())
            } else {
                name == g
            }
        })
    }
}

#[derive(Debug, Default)]
struct Audit {
    violations: Vec<String>,
    flagged: Vec<String>,
    locks: usize,
    bucks: usize,
    lock_packages: usize,
    workspaces: usize,
}

fn tracked(root: &Path, specs: &[&str]) -> Result<Vec<PathBuf>, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("cannot resolve audit root {}: {e}", root.display()))?;
    // Git mode only when the audit root IS the git top-level: inside a
    // larger git tree (e.g. an Eden mount nested in dev-hermit) git
    // discovery would name the OUTER repository, which is exactly the
    // wrong scope. Sapling is tried next; anything else fails closed.
    let git_top = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string())
                .canonicalize()
                .ok()
        });
    if git_top.as_deref() == Some(root.as_path()) {
        let mut args: Vec<&str> = vec!["ls-files", "--"];
        args.extend_from_slice(specs);
        let out = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(&args)
            .output()
            .map_err(|e| format!("cannot run git ls-files: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git ls-files failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let mut files: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| root.join(l.trim()))
            .collect();
        files.sort();
        files.dedup();
        return Ok(files);
    }
    // Sapling detection by marker walk, never by `sl` discovery: from
    // a subdirectory of an Eden mount nested inside another repo,
    // discovery can escape to the OUTER repository (measured: bare
    // `sl root` inside the mount named dev-hermit). The first ancestor
    // carrying a `.sl`, `.hg` or `.eden` marker is the candidate root, and
    // `sl -R <candidate> root` must confirm it before any use.
    // A subtree may carry its own stray marker (fbsource's hermit tree
    // ships an `.eden` directory that is not a repo root), so every
    // marker ancestor is tried until `sl -R` confirms one.
    let sl_root = root.ancestors().find_map(|candidate| {
        if !(candidate.join(".sl").is_dir()
            || candidate.join(".hg").is_dir()
            || candidate.join(".eden").is_dir())
        {
            return None;
        }
        let out = Command::new("sl")
            .arg("-R")
            .arg(candidate)
            .arg("root")
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let named = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
            .canonicalize()
            .ok()?;
        (named == candidate).then(|| candidate.to_path_buf())
    });
    if let Some(vcs_root) = sl_root {
        let rel = root.strip_prefix(&vcs_root).map_err(|_| {
            format!(
                "Sapling root {} is not an ancestor of audit root {}",
                vcs_root.display(),
                root.display()
            )
        })?;
        let scope: String = if rel.as_os_str().is_empty() {
            ".".to_string()
        } else {
            rel.display().to_string()
        };
        let mut files: Vec<PathBuf> = vec![];
        for spec in specs {
            // git pathspec `:(glob)**/X` and bare `X` both map to one
            // Sapling include glob over the audit subtree.
            let name = spec.strip_prefix(":(glob)**/").unwrap_or(spec);
            let include = format!("glob:**/{name}");
            let out = Command::new("sl")
                .current_dir(&vcs_root)
                .arg("-R")
                .arg(&vcs_root)
                .args(["files", &scope, "-I", &include])
                .output()
                .map_err(|e| format!("cannot run sl files: {e}"))?;
            // `sl files` returns 1 when nothing matches; that is an
            // empty answer for this spec, not an error.
            if out.status.code() == Some(1) {
                continue;
            }
            if !out.status.success() {
                return Err(format!(
                    "sl files failed in {}: {}",
                    root.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let l = line.trim();
                if !l.is_empty() {
                    files.push(vcs_root.join(l));
                }
            }
        }
        files.sort();
        files.dedup();
        return Ok(files);
    }
    Err(format!(
        "unknown VCS at {}: neither a git top-level nor a Sapling checkout; \
         refusing to guess the tracked-file scope",
        root.display()
    ))
}

fn judge(audit: &mut Audit, deny: &DenyList, today: &Date, name: &str, where_: &str) {
    if !deny.denies(name) {
        return;
    }
    match deny.allow_until.get(name) {
        Some((expiry, reason)) if today <= expiry => audit.flagged.push(format!(
            "{name} at {where_}: KNOWN VIOLATION, allow-listed until \
             {y:04}-{m:02}-{d:02} ({reason})",
            y = expiry.y,
            m = expiry.m,
            d = expiry.d
        )),
        Some((expiry, reason)) => audit.violations.push(format!(
            "{name} at {where_}: allow-until EXPIRED {y:04}-{m:02}-{d:02} ({reason})",
            y = expiry.y,
            m = expiry.m,
            d = expiry.d
        )),
        None => audit.violations.push(format!(
            "{name} at {where_}: denied package in Hermit's graph"
        )),
    }
}

/// The transitive closure of a package's `default` feature, from the
/// `features` map in cargo metadata. Entries may name features of the
/// same package, `dep:<name>` optional-dependency activations, or
/// `<dep>/<feature>` activations on dependencies; only same-package
/// feature names belong in the closure.
fn default_closure(features: &serde_json::Map<String, serde_json::Value>) -> BTreeSet<String> {
    let mut closure: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = vec!["default".to_string()];
    while let Some(f) = stack.pop() {
        if !closure.insert(f.clone()) {
            continue;
        }
        if let Some(serde_json::Value::Array(members)) = features.get(&f) {
            for m in members {
                if let Some(s) = m.as_str() {
                    if !s.starts_with("dep:") && !s.contains('/') {
                        stack.push(s.to_string());
                    }
                }
            }
        }
    }
    closure
}

/// `cargo` pinned past any `@fb-only` relative-path toolchain file:
/// on the fbsource side hermit's rust-toolchain.toml names a Buck-
/// provisioned toolchain path that plain cargo cannot use, so an
/// unpinned invocation dies before listing anything. Metadata only
/// lists packages, so honoring the manifest tree's own channel pin
/// (export side) or falling back to `stable` cannot change package
/// identity; a too-old cargo fails closed (rc=2) instead.
fn cargo_command(near: &Path) -> Command {
    let mut cmd = Command::new("cargo");
    if std::env::var_os("RUSTUP_TOOLCHAIN").is_none() {
        let channel = near
            .ancestors()
            .take(4)
            .filter_map(|a| std::fs::read_to_string(a.join("rust-toolchain.toml")).ok())
            .find_map(|text| {
                let doc: toml::Value = text.parse().ok()?;
                doc.get("toolchain")?
                    .get("channel")?
                    .as_str()
                    .map(|c| c.to_string())
            })
            .unwrap_or_else(|| "stable".to_string());
        cmd.env("RUSTUP_TOOLCHAIN", channel);
    }
    cmd
}

fn run_cargo_metadata(manifest: &Path) -> Result<serde_json::Value, String> {
    let out = cargo_command(manifest)
        .current_dir(manifest.parent().unwrap_or_else(|| Path::new(".")))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--manifest-path",
        ])
        .arg(manifest)
        .output()
        .map_err(|e| format!("cannot run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed for {}: {}",
            manifest.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| {
        format!(
            "cargo metadata output is not JSON for {}: {e}",
            manifest.display()
        )
    })
}

/// Which string at `bytes[i..]` is a quoted literal? Returns
/// (content, byte length including delimiters), handling Starlark's
/// single, double, and triple-quoted forms. Escapes are preserved in
/// the content; labels never contain them, so no unescaping is needed
/// for the equality checks below.
fn quoted_literal(bytes: &[u8], i: usize) -> Option<(String, usize)> {
    let quote = *bytes.get(i)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let triple = bytes.get(i + 1) == Some(&quote) && bytes.get(i + 2) == Some(&quote);
    let delim = if triple { 3 } else { 1 };
    let mut j = i + delim;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'\n' if !triple => return None,
            q if q == quote => {
                if triple {
                    if bytes.get(j + 1) == Some(&quote) && bytes.get(j + 2) == Some(&quote) {
                        let content = String::from_utf8_lossy(&bytes[i + delim..j]).into_owned();
                        return Some((content, j + delim - i));
                    }
                    j += 1;
                } else {
                    let content = String::from_utf8_lossy(&bytes[i + 1..j]).into_owned();
                    return Some((content, j + 1 - i));
                }
            }
            _ => j += 1,
        }
    }
    None
}

/// Replace `#` comments outside string literals with spaces, preserving
/// byte offsets (and therefore line numbers) of everything else.
fn buck_without_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            match quoted_literal(bytes, i) {
                Some((_, len)) => i += len,
                None => i += 1,
            }
        } else if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_dep_assignment(name: &str) -> bool {
    name == "deps"
        || name == "named_deps"
        || name == "exported_deps"
        || name == "runtime_deps"
        || name == "extra_buck_dependencies"
        || name == "dependencies"
        || name.ends_with("_deps")
        || name.ends_with("DEPS")
}

/// The Reverie target named by a Buck label literal, if it is one.
/// Only labels into the Reverie cell count: a denied *crate* is a
/// target there, while rule names, visibility strings, and local
/// `:targets` elsewhere are not graph edges into Reverie.
fn reverie_label_target(literal: &str) -> Option<String> {
    let label = literal
        .strip_prefix("fbsource//")
        .or_else(|| literal.strip_prefix("@fbcode//"))
        .unwrap_or(literal);
    let rest = label.strip_prefix("//hermetic_infra/reverie")?;
    let target = if let Some((_, target)) = rest.rsplit_once(':') {
        target
    } else {
        rest.rsplit('/').next().unwrap_or("")
    };
    (!target.is_empty() && target != "..." && !target.contains('*')).then(|| target.to_string())
}

/// Dependency-edge labels in one BUCK file: string literals inside
/// dependency-bearing assignments (`deps = [...]`, `named_deps = {...}`,
/// `*_DEPS = [...]`, ...), after comment removal. Returns
/// (target name, 1-based line) pairs. This is deliberately narrower
/// than a text scan until Slice 1b's buck2 query replaces it: a
/// denied name in a comment, visibility list, rule name, or unrelated
/// string is not an edge and must not refuse.
fn buck_dependency_labels(text: &str) -> Vec<(String, usize)> {
    let clean = buck_without_comments(text);
    let bytes = clean.as_bytes();
    let mut labels = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            match quoted_literal(bytes, i) {
                Some((_, len)) => {
                    i += len;
                    continue;
                }
                None => i += 1,
            }
            continue;
        }
        if !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let ident = &clean[start..i];
        let mut eq = i;
        while eq < bytes.len() && bytes[eq].is_ascii_whitespace() {
            eq += 1;
        }
        if !is_dep_assignment(ident)
            || bytes.get(eq) != Some(&b'=')
            || bytes.get(eq + 1) == Some(&b'=')
        {
            continue;
        }
        // Capture the assigned expression: brackets nest, a top-level
        // comma or an unmatched close-paren ends a rule attribute, and
        // a newline at depth zero ends a file-level assignment.
        let mut j = eq + 1;
        let mut depth: i32 = 0;
        let mut saw_bracket = false;
        while j < bytes.len() {
            match bytes[j] {
                b'"' | b'\'' => {
                    if let Some((literal, len)) = quoted_literal(bytes, j) {
                        if saw_bracket {
                            if let Some(target) = reverie_label_target(&literal) {
                                labels.push((
                                    target,
                                    clean[..j].bytes().filter(|b| *b == b'\n').count() + 1,
                                ));
                            }
                        }
                        j += len;
                        continue;
                    }
                    j += 1;
                }
                b'[' | b'{' | b'(' => {
                    saw_bracket = true;
                    depth += 1;
                    j += 1;
                }
                b']' | b'}' => {
                    depth -= 1;
                    j += 1;
                }
                b')' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    j += 1;
                }
                b',' if depth == 0 => break,
                b'\n' if depth == 0 && saw_bracket => break,
                _ => j += 1,
            }
        }
        i = j.max(i + 1);
    }
    labels
}

fn audit_root(root: &Path, deny: &DenyList, today: &Date) -> Result<Audit, String> {
    let mut audit = Audit::default();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let locks = tracked(root, &["Cargo.lock", ":(glob)**/Cargo.lock"])?;
    if locks.is_empty() {
        return Err("git tracks no Cargo.lock files".to_string());
    }

    // 1. All-platforms package census from each lockfile, TOML-parsed.
    for lock in &locks {
        audit.locks += 1;
        let rel = lock
            .strip_prefix(root)
            .unwrap_or(lock)
            .display()
            .to_string();
        let text = std::fs::read_to_string(lock)
            .map_err(|e| format!("cannot read {}: {e}", lock.display()))?;
        let doc: toml::Value = text
            .parse()
            .map_err(|e| format!("{} is not valid TOML: {e}", lock.display()))?;
        let packages = doc
            .get("package")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();
        for pkg in &packages {
            if let Some(name) = pkg.get("name").and_then(|n| n.as_str()) {
                audit.lock_packages += 1;
                if deny.denies(name) && seen.insert((name.to_string(), rel.clone())) {
                    judge(&mut audit, deny, today, name, &rel);
                }
            }
        }
    }

    // 2. Resolved graph per lockfile workspace: rename-proof package
    // identities and the core crate's enabled features.
    let mut manifests: BTreeSet<PathBuf> = BTreeSet::new();
    for lock in &locks {
        let manifest = lock.with_file_name("Cargo.toml");
        if manifest.is_file() {
            manifests.insert(manifest);
        }
    }
    for manifest in &manifests {
        audit.workspaces += 1;
        let rel = manifest
            .strip_prefix(root)
            .unwrap_or(manifest)
            .display()
            .to_string();
        let meta = run_cargo_metadata(manifest)?;
        let empty = serde_json::Value::Array(vec![]);
        let packages = meta.get("packages").unwrap_or(&empty);
        let mut by_id: BTreeMap<String, &serde_json::Value> = BTreeMap::new();
        if let Some(arr) = packages.as_array() {
            for pkg in arr {
                let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let id = pkg.get("id").and_then(|n| n.as_str()).unwrap_or("");
                by_id.insert(id.to_string(), pkg);
                if deny.denies(name) && seen.insert((name.to_string(), rel.clone())) {
                    judge(&mut audit, deny, today, name, &rel);
                }
            }
        }
        let nodes = meta
            .get("resolve")
            .and_then(|r| r.get("nodes"))
            .unwrap_or(&empty);
        if let Some(arr) = nodes.as_array() {
            for node in arr {
                let id = node.get("id").and_then(|n| n.as_str()).unwrap_or("");
                let Some(pkg) = by_id.get(id) else { continue };
                let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if name != CORE_CRATE {
                    continue;
                }
                let features_map = pkg
                    .get("features")
                    .and_then(|f| f.as_object())
                    .cloned()
                    .unwrap_or_default();
                let closure = default_closure(&features_map);
                let mut extra: Vec<String> = vec![];
                if let Some(serde_json::Value::Array(enabled)) = node.get("features") {
                    for f in enabled {
                        if let Some(s) = f.as_str() {
                            if !closure.contains(s) {
                                extra.push(s.to_string());
                            }
                        }
                    }
                }
                if !extra.is_empty() {
                    audit.violations.push(format!(
                        "{CORE_CRATE} non-default feature(s) [{}] enabled in workspace {rel}: \
                         core features unify into every build",
                        extra.join(", ")
                    ));
                }
            }
        }
    }

    // 3. BUCK dependency-edge labels (Slice 1b replaces this with a
    // buck2 query; until then only labels in dependency-bearing
    // assignments count, never bare mentions or comments).
    for buck in tracked(root, &["BUCK", ":(glob)**/BUCK"])? {
        audit.bucks += 1;
        let rel = buck
            .strip_prefix(root)
            .unwrap_or(&buck)
            .display()
            .to_string();
        let text = std::fs::read_to_string(&buck)
            .map_err(|e| format!("cannot read {}: {e}", buck.display()))?;
        for (name, line) in buck_dependency_labels(&text) {
            if deny.denies(&name) && seen.insert((name.clone(), format!("{rel}:{line}"))) {
                judge(&mut audit, deny, today, &name, &format!("{rel}:{line}"));
            }
        }
    }
    Ok(audit)
}

fn report(audit: &Audit) -> i32 {
    for f in &audit.flagged {
        println!("FLAGGED (expiring debt): {f}");
    }
    if audit.violations.is_empty() {
        println!(
            "PASS (BUCK files only; .bzl macros and extraction forms 1-4 not scanned; see Slice 1b): no denied LinuxEmu extension package in Hermit's graph \
             ({} Cargo.lock [{} packages], {} cargo-metadata workspace(s), {} BUCK; \
             {} flagged allow-until debt).",
            audit.locks,
            audit.lock_packages,
            audit.workspaces,
            audit.bucks,
            audit.flagged.len()
        );
        RC_PASS
    } else {
        for v in &audit.violations {
            eprintln!("REFUSE: {v}");
        }
        eprintln!(
            "REFUSE: {} denied hit(s). Hermit builds only the LinuxEmu core; \
             see scripts/linuxemu-graph-deny.txt.",
            audit.violations.len()
        );
        RC_REFUSE
    }
}

fn report_code(audit: &Audit) -> i32 {
    if audit.violations.is_empty() {
        RC_PASS
    } else {
        RC_REFUSE
    }
}

/// Write a scratch fixture tree, `git add` it (audit scope is tracked
/// files), generate its lockfile when it has manifests, and audit it.
/// Used only by the negative controls.
#[derive(Clone, Copy, PartialEq)]
enum ControlVcs {
    Git,
    Sl,
    None,
}

fn control_audit(
    files: &[(&str, &str)],
    deny: &DenyList,
    today: &Date,
    vcs: ControlVcs,
) -> Result<i32, String> {
    let dir = std::env::temp_dir().join(format!("linuxemu-graph-control-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let run = |dir: &Path| -> Result<i32, String> {
        for (name, body) in files {
            let path = dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("control mkdir: {e}"))?;
            }
            std::fs::write(&path, body).map_err(|e| format!("control write: {e}"))?;
        }
        match vcs {
            ControlVcs::Git => {
                for args in [vec!["init", "-q"], vec!["add", "-A"]] {
                    let ok = Command::new("git")
                        .arg("-C")
                        .arg(dir)
                        .args(&args)
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false);
                    if !ok {
                        return Err("control git setup failed".to_string());
                    }
                }
            }
            ControlVcs::Sl => {
                // `init` creates the repo, so it cannot take `-R` yet.
                for args in [vec!["init", "."], vec!["-R", ".", "add"]] {
                    let ok = Command::new("sl")
                        .current_dir(dir)
                        .args(&args)
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false);
                    if !ok {
                        return Err("control sl setup failed".to_string());
                    }
                }
            }
            ControlVcs::None => {}
        }
        if dir.join("Cargo.toml").is_file() {
            let ok = cargo_command(dir)
                .current_dir(dir)
                .args(["generate-lockfile", "--offline", "--manifest-path"])
                .arg(dir.join("Cargo.toml"))
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !ok {
                return Err("control generate-lockfile failed".to_string());
            }
            match vcs {
                ControlVcs::Git => {
                    let _ = Command::new("git")
                        .arg("-C")
                        .arg(dir)
                        .args(["add", "-A"])
                        .output();
                }
                ControlVcs::Sl => {
                    let _ = Command::new("sl")
                        .current_dir(dir)
                        .arg("-R")
                        .arg(dir)
                        .arg("add")
                        .output();
                }
                ControlVcs::None => {}
            }
        }
        audit_root(dir, deny, today).map(|a| report_code(&a))
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("control mkdir: {e}"))?;
    let result = run(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn sl_available() -> bool {
    Command::new("sl")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn run_ok(mut command: Command) -> bool {
    command.output().is_ok_and(|output| output.status.success())
}

fn bare_sl_root(dir: &Path) -> Result<PathBuf, String> {
    let output = Command::new("sl")
        .current_dir(dir)
        .arg("root")
        .output()
        .map_err(|error| format!("cannot run bare sl root in {}: {error}", dir.display()))?;
    if !output.status.success() {
        return Err(format!(
            "bare sl root failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        .canonicalize()
        .map_err(|error| format!("cannot resolve bare sl root for {}: {error}", dir.display()))
}

/// The Eden discovery hazard as a portable fixture: an outer *git*
/// repository contains a Sapling checkout whose audit subtree carries
/// the Eden symlink shape found in a real mount. Bare `sl root`
/// resolves to the outer git repository (the escape), while the audit
/// must select only the inner Sapling scope. The outer lockfile is
/// poisoned with a denied package, so selecting the outer scope would
/// refuse; passing proves the marker walk selected the inner root.
fn nested_eden_escape_control(deny: &DenyList, today: &Date) -> Result<i32, String> {
    let base = std::env::temp_dir().join(format!("linuxemu-graph-nested-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let run = |base: &Path| -> Result<i32, String> {
        let outer = base.join("outer");
        let inner = outer.join("inner");
        let audit = inner.join("audit");
        for dir in [
            outer.as_path(),
            inner.as_path(),
            audit.join("consumer/src").as_path(),
            audit.join("stub/src").as_path(),
            base.join("eden-marker").as_path(),
        ] {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("nested control mkdir {}: {error}", dir.display()))?;
        }
        std::fs::write(
            outer.join("Cargo.lock"),
            "[[package]]\nname = \"reverie-linuxemu-ext-demo\"\nversion = \"0.0.0\"\n",
        )
        .map_err(|error| format!("nested control write outer lock: {error}"))?;
        std::fs::write(
            audit.join("Cargo.toml"),
            "[workspace]\nmembers = [\"consumer\", \"stub\"]\nresolver = \"2\"\n",
        )
        .map_err(|error| format!("nested control write workspace: {error}"))?;
        std::fs::write(
            audit.join("consumer/Cargo.toml"),
            "[package]\nname = \"consumer\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[dependencies]\nstub = { path = \"../stub\" }\n",
        )
        .map_err(|error| format!("nested control write consumer: {error}"))?;
        std::fs::write(
            audit.join("stub/Cargo.toml"),
            "[package]\nname = \"stub\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .map_err(|error| format!("nested control write stub: {error}"))?;
        std::fs::write(audit.join("consumer/src/lib.rs"), "")
            .map_err(|error| format!("nested control write consumer lib: {error}"))?;
        std::fs::write(audit.join("stub/src/lib.rs"), "")
            .map_err(|error| format!("nested control write stub lib: {error}"))?;
        #[cfg(unix)]
        {
            // The audit subtree's Eden symlink is the salient shape;
            // a synthetic mount-root `.eden` would make plain Sapling
            // refuse its own working copy as corrupt Eden metadata.
            std::os::unix::fs::symlink(base.join("eden-marker"), audit.join(".eden"))
                .map_err(|error| format!("nested control Eden audit symlink: {error}"))?;
        }
        if !run_ok({
            let mut command = Command::new("git");
            command.arg("-C").arg(&outer).args(["init", "-q"]);
            command
        }) || !run_ok({
            let mut command = Command::new("git");
            command.arg("-C").arg(&outer).args(["add", "Cargo.lock"]);
            command
        }) {
            return Err("nested control outer git setup failed".to_string());
        }
        if !run_ok({
            let mut command = Command::new("sl");
            command.current_dir(&inner).args(["init", "."]);
            command
        }) {
            return Err("nested control inner sl init failed".to_string());
        }
        if !run_ok({
            let mut command = cargo_command(&audit);
            command
                .current_dir(&audit)
                .args(["generate-lockfile", "--offline", "--manifest-path"])
                .arg(audit.join("Cargo.toml"));
            command
        }) {
            return Err("nested control generate-lockfile failed".to_string());
        }
        if !run_ok({
            let mut command = Command::new("sl");
            command.current_dir(&inner).arg("-R").arg(&inner).arg("add");
            command
        }) {
            return Err("nested control inner sl add failed".to_string());
        }
        let outer = outer
            .canonicalize()
            .map_err(|error| format!("nested control outer resolve: {error}"))?;
        let inner = inner
            .canonicalize()
            .map_err(|error| format!("nested control inner resolve: {error}"))?;
        let bare = bare_sl_root(&audit)?;
        if bare != outer {
            return Err(format!(
                "nested control did not reproduce the Eden escape: bare sl root in {} gave {}, want outer {} (inner is {})",
                audit.display(),
                bare.display(),
                outer.display(),
                inner.display()
            ));
        }
        audit_root(&audit, deny, today).map(|audit| report_code(&audit))
    };
    std::fs::create_dir_all(&base)
        .map_err(|error| format!("nested control mkdir {}: {error}", base.display()))?;
    let result = run(&base);
    let _ = std::fs::remove_dir_all(&base);
    result
}

fn negative_controls(deny: &DenyList, today: &Date) -> Result<(), String> {
    const WS: &str = "[workspace]\nmembers = [\"consumer\", \"stub\"]\nresolver = \"2\"\n";
    const STUB_LIB: &str = "";
    let stub_pkg = |name: &str, extra: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n{extra}")
    };
    let consumer = |dep: &str| {
        format!(
            "[package]\nname = \"consumer\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[dependencies]\n{dep}\n"
        )
    };
    let clean: Vec<(&str, String)> = vec![
        ("Cargo.toml", WS.to_string()),
        (
            "consumer/Cargo.toml",
            consumer("stub = { path = \"../stub\" }"),
        ),
        ("consumer/src/lib.rs", STUB_LIB.to_string()),
        ("stub/Cargo.toml", stub_pkg("stub", "")),
        ("stub/src/lib.rs", STUB_LIB.to_string()),
    ];
    let ext_planted: Vec<(&str, String)> = vec![
        ("Cargo.toml", WS.to_string()),
        (
            "consumer/Cargo.toml",
            consumer("reverie-linuxemu-ext-demo = { path = \"../stub\" }"),
        ),
        ("consumer/src/lib.rs", STUB_LIB.to_string()),
        ("stub/Cargo.toml", stub_pkg("reverie-linuxemu-ext-demo", "")),
        ("stub/src/lib.rs", STUB_LIB.to_string()),
    ];
    // Seat bypass (a): features on its own line(s), spread table.
    let feature_own_line: Vec<(&str, String)> = vec![
        ("Cargo.toml", WS.to_string()),
        (
            "consumer/Cargo.toml",
            consumer(
                "reverie-linuxemu = {\n    path = \"../stub\",\n    features = [\n        \"test-support\",\n    ],\n}",
            ),
        ),
        ("consumer/src/lib.rs", STUB_LIB.to_string()),
        (
            "stub/Cargo.toml",
            stub_pkg(CORE_CRATE, "[features]\ndefault = []\ntest-support = []\n"),
        ),
        ("stub/src/lib.rs", STUB_LIB.to_string()),
    ];
    // Seat bypass (b): multiline renamed key naming a denied package.
    let renamed_multiline: Vec<(&str, String)> = vec![
        ("Cargo.toml", WS.to_string()),
        (
            "consumer/Cargo.toml",
            consumer(
                "renamed = {\n    package =\n        \"reverie-linuxemu-ext-demo\",\n    path = \"../stub\",\n}",
            ),
        ),
        ("consumer/src/lib.rs", STUB_LIB.to_string()),
        ("stub/Cargo.toml", stub_pkg("reverie-linuxemu-ext-demo", "")),
        ("stub/src/lib.rs", STUB_LIB.to_string()),
    ];
    // A same-named optional dependency activated by default through
    // `dep:procfs` must not make the distinct non-default feature
    // `procfs` look default (General Reviewer, D132127408).
    let feature_dep_collision: Vec<(&str, String)> = vec![
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"consumer\", \"stub\", \"procfsdep\"]\nresolver = \"2\"\n"
                .to_string(),
        ),
        (
            "consumer/Cargo.toml",
            consumer(
                "reverie-linuxemu = { path = \"../stub\", features = [\"procfs\"] }",
            ),
        ),
        ("consumer/src/lib.rs", STUB_LIB.to_string()),
        (
            "stub/Cargo.toml",
            "[package]\nname = \"reverie-linuxemu\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[dependencies]\nprocfs = { path = \"../procfsdep\", optional = true }\n[features]\ndefault = [\"dep:procfs\"]\nprocfs = []\n"
                .to_string(),
        ),
        ("stub/src/lib.rs", STUB_LIB.to_string()),
        ("procfsdep/Cargo.toml", stub_pkg("procfs", "")),
        ("procfsdep/src/lib.rs", STUB_LIB.to_string()),
    ];
    // BUCK mentions that are not dependency edges (comments, inline
    // comments, visibility, rule names, unrelated strings) must pass;
    // the same names as a real deps label must refuse.
    let mut buck_nonedge = clean.clone();
    buck_nonedge.push((
        "BUCK",
        "# reverie-linuxemu-ext-demo reverie-linuxemu-full reverie-procfs\nrust_library(\n    name = \"reverie-linuxemu-ext-demo\",\n    srcs = [\"x.rs\"],  # TODO drop reverie-linuxemu-full later\n    visibility = [\"//hermetic_infra/reverie:reverie-linuxemu-ext-demo\"],\n    note = \"reverie-procfs\",\n)\n"
            .to_string(),
    ));
    let mut buck_edge = clean.clone();
    buck_edge.push((
        "BUCK",
        "rust_library(\n    name = \"consumer\",\n    deps = [\n        \"//hermetic_infra/reverie:reverie-linuxemu-ext-demo\",\n    ],\n)\n"
            .to_string(),
    ));
    const LOCK_PROCFS: &str = "[[package]]\nname = \"reverie-procfs\"\nversion = \"0.5.7\"\n";
    let cases: Vec<(&str, Vec<(&str, String)>, Date, ControlVcs, i32)> = vec![
        (
            "clean fixture passes (git)",
            clean.clone(),
            today.clone(),
            ControlVcs::Git,
            RC_PASS,
        ),
        (
            "planted extension refused",
            ext_planted,
            today.clone(),
            ControlVcs::Git,
            RC_REFUSE,
        ),
        (
            "features on own line refused",
            feature_own_line,
            today.clone(),
            ControlVcs::Git,
            RC_REFUSE,
        ),
        (
            "multiline renamed key refused",
            renamed_multiline,
            today.clone(),
            ControlVcs::Git,
            RC_REFUSE,
        ),
        (
            "expired allow-until refused",
            vec![("Cargo.lock", LOCK_PROCFS.to_string())],
            Date {
                y: 9999,
                m: 1,
                d: 1,
            },
            ControlVcs::Git,
            RC_REFUSE,
        ),
        (
            "dep: name does not hide same-named feature",
            feature_dep_collision,
            today.clone(),
            ControlVcs::Git,
            RC_REFUSE,
        ),
        (
            "BUCK non-edge mentions pass",
            buck_nonedge,
            today.clone(),
            ControlVcs::Git,
            RC_PASS,
        ),
        (
            "BUCK dependency edge refused",
            buck_edge,
            today.clone(),
            ControlVcs::Git,
            RC_REFUSE,
        ),
        (
            "clean fixture passes (sl)",
            clean,
            today.clone(),
            ControlVcs::Sl,
            RC_PASS,
        ),
    ];
    let sl_present = sl_available();
    let mut sapling_skips = 0usize;
    for (name, files, when, vcs, want) in &cases {
        if *vcs == ControlVcs::Sl && !sl_present {
            sapling_skips += 1;
            println!("SKIP (counted): negative control {name:?} requires sl, which is absent");
            continue;
        }
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (*a, b.as_str())).collect();
        let got = control_audit(&refs, deny, when, *vcs)?;
        if got != *want {
            return Err(format!(
                "negative control {name:?} gave rc={got}, want rc={want}; refusing to trust this checker run"
            ));
        }
    }
    if sl_present {
        let got = nested_eden_escape_control(deny, today)?;
        if got != RC_PASS {
            return Err(format!(
                "negative control \"nested Eden escape selects inner Sapling root\" gave rc={got}, want rc={RC_PASS}; refusing to trust this checker run"
            ));
        }
    } else {
        sapling_skips += 1;
        println!(
            "SKIP (counted): negative control \"nested Eden escape selects inner Sapling root\" requires sl, which is absent"
        );
    }
    if sapling_skips > 0 {
        println!(
            "SKIP SUMMARY (counted): {sapling_skips} Sapling negative control(s) skipped because sl is absent; git controls ran"
        );
    }
    // Unknown VCS must fail closed (Err -> CHECKER ERROR rc=2 in main),
    // never guess a tracked-file scope.
    let lock_only: Vec<(&str, &str)> = vec![("Cargo.lock", LOCK_PROCFS)];
    match control_audit(&lock_only, deny, today, ControlVcs::None) {
        Err(_) => {}
        Ok(rc) => {
            return Err(format!(
                "negative control \"unknown VCS fails closed\" gave rc={rc}, want Err"
            ));
        }
    }
    Ok(())
}
fn main() {
    rust_script_prelude::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root = PathBuf::from(".");
    let mut deny_file: Option<PathBuf> = None;
    let mut skip_controls = false;
    let mut today: Option<Date> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--repo-root" => {
                i += 1;
                root = PathBuf::from(args.get(i).cloned().unwrap_or_default());
            }
            "--deny-file" => {
                i += 1;
                deny_file = Some(PathBuf::from(args.get(i).cloned().unwrap_or_default()));
            }
            "--today" => {
                i += 1;
                today = Some(
                    match Date::parse(args.get(i).map(|s| s.as_str()).unwrap_or("")) {
                        Ok(d) => d,
                        Err(e) => {
                            eprintln!("CHECKER ERROR: --today: {e}");
                            std::process::exit(RC_ERROR);
                        }
                    },
                );
            }
            "--skip-negative-control" => skip_controls = true,
            "-h" | "--help" => {
                println!(
                    "usage: check-linuxemu-graph.rs [--repo-root PATH] [--deny-file PATH]\n\
                     \x20      [--today YYYY-MM-DD] [--skip-negative-control]\n\
                     rc: 0 PASS, 1 REFUSE (denied package/feature), 2 CHECKER ERROR."
                );
                std::process::exit(RC_PASS);
            }
            other => {
                eprintln!("CHECKER ERROR: unknown argument {other:?} (see --help)");
                std::process::exit(RC_ERROR);
            }
        }
        i += 1;
    }
    let deny_path = deny_file.unwrap_or_else(|| root.join("scripts/linuxemu-graph-deny.txt"));
    let deny = match DenyList::load(&deny_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("CHECKER ERROR: {e}");
            std::process::exit(RC_ERROR);
        }
    };
    let today = match today.or_else(|| Date::today().ok()) {
        Some(d) => d,
        None => {
            eprintln!("CHECKER ERROR: cannot determine today's date");
            std::process::exit(RC_ERROR);
        }
    };
    if !skip_controls {
        if let Err(e) = negative_controls(&deny, &today) {
            eprintln!("CHECKER ERROR: {e}");
            std::process::exit(RC_ERROR);
        }
    }
    match audit_root(&root, &deny, &today) {
        Ok(audit) => std::process::exit(report(&audit)),
        Err(e) => {
            eprintln!("CHECKER ERROR: {e}");
            std::process::exit(RC_ERROR);
        }
    }
}
