#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Affected-test selection for Hermit CI.
//!
//! Given a commit's changed files, decide which portable-DAG nodes actually
//! need to run. Three outcomes:
//!
//!   * `skip`      — every changed file is provably CI-irrelevant (docs/notes);
//!                   run nothing.
//!   * `selective` — changed files map to a subset of the suite; run that
//!                   subset plus its transitive build dependencies and the
//!                   always-on preflight gates.
//!   * `full`      — a changed file forces the whole suite (build config,
//!                   toolchain, the CI harness itself) OR maps to no known rule
//!                   (unknown ⇒ conservative full).
//!
//! FAIL-SAFE: the only outcome that runs *fewer* tests than the current commit
//! could theoretically need is `skip`, and `skip` requires positive proof that
//! EVERY file is inert. Any doubt resolves to `full`. A mismapped footprint can
//! therefore only waste time, never hide a regression.
//!
//! The node universe, node dependencies, and node commands are read from
//! `ci/dag/portable.json` (single source of truth); the path→node relation is
//! read from `ci/test-footprints.json`.
//!
//! Usage:
//!   ci/select-tests.rs --base origin/main            # diff HEAD against base
//!   git diff --name-only A B | ci/select-tests.rs --files -
//!   ci/select-tests.rs --files path/a.rs path/b.md   # explicit file list
//!   ci/select-tests.rs --base origin/main --format github   # emit GITHUB_OUTPUT
//!   ci/select-tests.rs --self-test                   # run built-in unit tests
//!
//! Output (default `--format human`): a decision line + the selected node set.
//! `--format json` prints a machine-readable object. `--format github` appends
//! `decision=`, `skip=`, `full=`, `node_count=`, and `nodes=` (space-joined) to
//! `$GITHUB_OUTPUT` (or stdout if unset), so a workflow can gate its matrix.
//!
//! Partial Cargo manifest:
//!
//! ```cargo
//! [dependencies]
//! serde_json = "1.0"
//! ```

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

// ---------------------------------------------------------------------------
// Glob matching (dependency-free, git-pathspec-like)
// ---------------------------------------------------------------------------

/// Match `text` against a glob `pattern` over a `/`-separated path.
///
/// Supported syntax:
///   * `**` matches any run of characters, including `/` (spans directories).
///   * `*`  matches any run of characters except `/`.
///   * `?`  matches exactly one character except `/`.
///   * every other byte is a literal.
///
/// A pattern with no `/` (e.g. `Cargo.toml`) matches only at that exact path,
/// not as a suffix, so root-level `Cargo.toml` is distinct from
/// `detcore/Cargo.toml`.
fn glob_match(pattern: &str, text: &str) -> bool {
    glob_inner(pattern.as_bytes(), text.as_bytes())
}

fn glob_inner(p: &[u8], t: &[u8]) -> bool {
    // Clean recursive matcher. Patterns and paths are short, so the worst-case
    // branching is irrelevant in practice and correctness is easy to see.
    if p.is_empty() {
        return t.is_empty();
    }
    if p[0] == b'*' {
        if p.len() >= 2 && p[1] == b'*' {
            // `**` — matches any run of characters, including `/`.
            let mut rest = &p[2..];
            if rest.first() == Some(&b'/') {
                // `**/` may also match zero directories (a/**/b matches a/b).
                rest = &rest[1..];
            }
            let mut i = 0;
            loop {
                if glob_inner(rest, &t[i..]) {
                    return true;
                }
                if i >= t.len() {
                    return false;
                }
                i += 1;
            }
        } else {
            // Single `*` — matches any run of non-`/` characters.
            let mut i = 0;
            loop {
                if glob_inner(&p[1..], &t[i..]) {
                    return true;
                }
                if i >= t.len() || t[i] == b'/' {
                    return false;
                }
                i += 1;
            }
        }
    } else if p[0] == b'?' {
        !t.is_empty() && t[0] != b'/' && glob_inner(&p[1..], &t[1..])
    } else {
        !t.is_empty() && p[0] == t[0] && glob_inner(&p[1..], &t[1..])
    }
}

// ---------------------------------------------------------------------------
// Footprint model
// ---------------------------------------------------------------------------

/// One footprint entry: which paths map to which node refs, plus optional e2e
/// cell affinity (`e2e_all` = every backend's cells; `e2e_backends` = only those
/// backends' cells).
struct Fp {
    paths: Vec<String>,
    nodes: Vec<String>,
    e2e_all: bool,
    e2e_backends: Vec<String>,
}

struct Footprints {
    groups: BTreeMap<String, Vec<String>>,
    force_full: Vec<String>,
    ci_irrelevant: Vec<String>,
    footprints: Vec<Fp>,
}

impl Footprints {
    fn load(path: &Path) -> Footprints {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", path.display())));
        let v: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| fail(&format!("invalid JSON in {}: {e}", path.display())));

        let groups = v["groups"]
            .as_object()
            .map(|o| {
                o.iter()
                    .map(|(k, val)| (k.clone(), str_vec(val)))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();

        let force_full = str_vec(&v["force_full"]);
        let ci_irrelevant = str_vec(&v["ci_irrelevant"]);

        let footprints = v["footprints"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|fp| Fp {
                        paths: str_vec(&fp["paths"]),
                        nodes: str_vec(&fp["nodes"]),
                        e2e_all: fp["e2e_all"].as_bool().unwrap_or(false),
                        e2e_backends: str_vec(&fp["e2e_backends"]),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Footprints { groups, force_full, ci_irrelevant, footprints }
    }

    /// Expand a node-reference list, resolving `@GROUP` aliases recursively.
    fn expand(&self, refs: &[String], out: &mut BTreeSet<String>, seen: &mut BTreeSet<String>) {
        for r in refs {
            if let Some(name) = r.strip_prefix('@') {
                if !seen.insert(name.to_string()) {
                    continue; // guard against alias cycles
                }
                match self.groups.get(name) {
                    Some(members) => self.expand(members, out, seen),
                    None => fail(&format!("footprints: unknown group @{name}")),
                }
            } else {
                out.insert(r.clone());
            }
        }
    }
}

fn str_vec(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// DAG model (node universe + build-dependency closure)
// ---------------------------------------------------------------------------

struct Dag {
    all_nodes: BTreeSet<String>,
    deps: BTreeMap<String, Vec<String>>,
}

impl Dag {
    fn load(path: &Path) -> Dag {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", path.display())));
        let v: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| fail(&format!("invalid JSON in {}: {e}", path.display())));
        let mut all_nodes = BTreeSet::new();
        let mut deps = BTreeMap::new();
        for s in v["steps"].as_array().unwrap_or(&vec![]) {
            let tag = format!(
                "{}.{}",
                s["group"].as_str().unwrap_or(""),
                s["job"].as_str().unwrap_or("")
            );
            deps.insert(tag.clone(), str_vec(&s["deps"]));
            all_nodes.insert(tag);
        }
        Dag { all_nodes, deps }
    }

    /// Add every transitive `deps` predecessor of the given nodes.
    fn close_over_deps(&self, nodes: &BTreeSet<String>) -> BTreeSet<String> {
        let mut out = nodes.clone();
        let mut stack: Vec<String> = nodes.iter().cloned().collect();
        while let Some(n) = stack.pop() {
            if let Some(ds) = self.deps.get(&n) {
                for d in ds {
                    if out.insert(d.clone()) {
                        stack.push(d.clone());
                    }
                }
            }
        }
        out
    }
}

// Always-on cheap safety gates for any selective run.
const PREFLIGHT: &[&str] = &[
    "check.backend_abstraction",
    "check.portability_paths",
    "lint.rustfmt",
];

// ---------------------------------------------------------------------------
// Shard model (ci/portable-shards.json) + e2e cell plan (expected-e2e-plan.json)
// ---------------------------------------------------------------------------

/// One test shard: a workflow matrix cell running a set of DAG nodes.
/// `needs` is the release-build dependency ("dbi", "aux") or empty for debug.
struct Shard {
    slug: String,
    needs: String,
    nodes: Vec<String>,
}

struct Shards {
    debug: Vec<Shard>,
    release: Vec<Shard>,
}

impl Shards {
    fn load(path: &Path) -> Shards {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", path.display())));
        let v: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| fail(&format!("invalid JSON in {}: {e}", path.display())));
        let parse = |key: &str| -> Vec<Shard> {
            v[key]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|s| Shard {
                            slug: s["slug"].as_str().unwrap_or("").to_string(),
                            needs: s["needs"].as_str().unwrap_or("").to_string(),
                            nodes: str_vec(&s["nodes"]),
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        Shards { debug: parse("debug_shards"), release: parse("release_shards") }
    }
}

/// One e2e cell: a (category, mode, backend) tuple from the audited plan.
#[derive(Clone)]
struct Cell {
    category: String,
    mode: String,
    backend: String,
}

struct Plan {
    cells: Vec<Cell>,
}

impl Plan {
    fn load(path: &Path) -> Plan {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", path.display())));
        let v: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| fail(&format!("invalid JSON in {}: {e}", path.display())));
        let cells = v["cells"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|c| Cell {
                        category: c["category"].as_str().unwrap_or("").to_string(),
                        mode: c["mode"].as_str().unwrap_or("").to_string(),
                        backend: c["backend"].as_str().unwrap_or("naked").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Plan { cells }
    }

    fn slug(c: &Cell) -> String {
        format!("{}__{}__{}", c.category, c.mode, c.backend)
    }
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Decision {
    Skip,
    Selective,
    Full,
}

struct Selection {
    decision: Decision,
    nodes: BTreeSet<String>,
    /// Whether every backend's e2e cells may be affected (core/CLI/fixture change).
    e2e_all: bool,
    /// Backends whose e2e cells this change can affect (when not e2e_all).
    e2e_backends: BTreeSet<String>,
    reasons: Vec<String>,
}

fn matches_any(globs: &[String], file: &str) -> bool {
    globs.iter().any(|g| glob_match(g, file))
}

fn select(fp: &Footprints, dag: &Dag, files: &[String]) -> Selection {
    let mut reasons = Vec::new();

    // Full-suite result: every node, and every e2e cell (e2e_all).
    let full = |nodes: BTreeSet<String>, reasons: Vec<String>| Selection {
        decision: Decision::Full,
        nodes,
        e2e_all: true,
        e2e_backends: BTreeSet::new(),
        reasons,
    };

    if files.is_empty() {
        return full(
            dag.all_nodes.clone(),
            vec!["no changed-file information available → full suite".into()],
        );
    }

    let mut matched: BTreeSet<String> = BTreeSet::new();
    let mut force = false;
    let mut all_inert = true; // every file is ci_irrelevant with no footprint hit
    let mut unknown: Vec<String> = Vec::new();
    let mut e2e_all = false;
    let mut e2e_backends: BTreeSet<String> = BTreeSet::new();

    for f in files {
        if matches_any(&fp.force_full, f) {
            force = true;
            all_inert = false;
            reasons.push(format!("{f} → force_full"));
            continue;
        }
        let mut hit = false;
        for entry in &fp.footprints {
            if matches_any(&entry.paths, f) {
                let mut seen = BTreeSet::new();
                fp.expand(&entry.nodes, &mut matched, &mut seen);
                if entry.e2e_all {
                    e2e_all = true;
                }
                for b in &entry.e2e_backends {
                    e2e_backends.insert(b.clone());
                }
                hit = true;
            }
        }
        if hit {
            all_inert = false;
            continue;
        }
        if matches_any(&fp.ci_irrelevant, f) {
            continue; // inert; leaves all_inert as-is
        }
        // Matched no rule at all → unknown → conservative full.
        unknown.push(f.clone());
        all_inert = false;
    }

    if force {
        return full(dag.all_nodes.clone(), reasons);
    }
    if !unknown.is_empty() {
        reasons.push(format!(
            "{} unmapped path(s) (e.g. {}) → conservative full suite",
            unknown.len(),
            unknown.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        ));
        return full(dag.all_nodes.clone(), reasons);
    }
    if matched.is_empty() && all_inert {
        reasons.push("all changed files are CI-irrelevant → skip CI".into());
        return Selection {
            decision: Decision::Skip,
            nodes: BTreeSet::new(),
            e2e_all: false,
            e2e_backends: BTreeSet::new(),
            reasons,
        };
    }
    if matched.is_empty() {
        // Defensive: no match, not proven inert. Fail safe.
        reasons.push("no footprint matched but files not proven inert → full suite".into());
        return full(dag.all_nodes.clone(), reasons);
    }

    // Selective: add preflight, then close over build deps.
    for pf in PREFLIGHT {
        if dag.all_nodes.contains(*pf) {
            matched.insert((*pf).to_string());
        }
    }
    // Drop any footprint node that is not in the current DAG (schema drift guard).
    let unknown_nodes: Vec<String> =
        matched.iter().filter(|n| !dag.all_nodes.contains(*n)).cloned().collect();
    if !unknown_nodes.is_empty() {
        reasons.push(format!(
            "footprint referenced {} node(s) absent from portable.json ({}) → full suite (stale map)",
            unknown_nodes.len(),
            unknown_nodes.join(", ")
        ));
        return full(dag.all_nodes.clone(), reasons);
    }
    let closed = dag.close_over_deps(&matched);
    reasons.push(format!(
        "{} node(s) selected + deps → {} of {} nodes",
        matched.len(),
        closed.len(),
        dag.all_nodes.len()
    ));
    if e2e_all {
        reasons.push("e2e: all backends (core/CLI/fixture change)".into());
    } else if !e2e_backends.is_empty() {
        reasons.push(format!(
            "e2e: backend-scoped → {}",
            e2e_backends.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    } else {
        reasons.push("e2e: no cells (non-e2e change)".into());
    }
    Selection { decision: Decision::Selective, nodes: closed, e2e_all, e2e_backends, reasons }
}

// ---------------------------------------------------------------------------
// Shard + cell derivation (footprint → shard-selection layer)
// ---------------------------------------------------------------------------

/// A concrete run plan: which test shards and which e2e cells to execute, plus
/// the release-build jobs those selections require.
struct RunPlan {
    shards: Vec<String>,      // shard slugs (debug + release) to run
    cells: Vec<Cell>,         // e2e cells to run
    build_debug: bool,        // the shared debug build job
    build_dbi: bool,          // the DBI release artifact
    build_aux: bool,          // the sabre/liteinst release artifacts
    total_shards: usize,      // universe sizes, for reporting
    total_cells: usize,
}

/// Map a node-level Selection onto shards and e2e cells.
///
/// A test shard runs iff any of its nodes is in the selected set. An e2e cell
/// runs iff the change is e2e_all, or the cell's backend is in the selection's
/// backend set. Build jobs are pulled in only when a selected shard/cell needs
/// them. `full` runs everything; `skip` runs nothing.
fn derive_run_plan(sel: &Selection, shards: &Shards, plan: &Plan) -> RunPlan {
    let total_shards = shards.debug.len() + shards.release.len();
    let total_cells = plan.cells.len();

    let (shard_slugs, cells): (Vec<String>, Vec<Cell>) = match sel.decision {
        Decision::Skip => (Vec::new(), Vec::new()),
        Decision::Full => (
            shards.debug.iter().chain(&shards.release).map(|s| s.slug.clone()).collect(),
            plan.cells.clone(),
        ),
        Decision::Selective => {
            let runs = |s: &Shard| s.nodes.iter().any(|n| sel.nodes.contains(n));
            let slugs: Vec<String> = shards
                .debug
                .iter()
                .chain(&shards.release)
                .filter(|s| runs(s))
                .map(|s| s.slug.clone())
                .collect();
            let cells: Vec<Cell> = plan
                .cells
                .iter()
                .filter(|c| sel.e2e_all || sel.e2e_backends.contains(&c.backend))
                .cloned()
                .collect();
            (slugs, cells)
        }
    };

    // Which release-build artifacts do the selected release shards / cells need?
    let mut build_dbi = false;
    let mut build_aux = false;
    for s in &shards.release {
        if shard_slugs.contains(&s.slug) {
            match s.needs.as_str() {
                "dbi" => build_dbi = true,
                "aux" => build_aux = true,
                _ => {}
            }
        }
    }
    for c in &cells {
        match c.backend.as_str() {
            "dbi" | "sabre" => build_dbi = true,
            "liteinst" => build_aux = true,
            _ => {}
        }
    }
    // Anything running at all needs the shared debug build.
    let build_debug = !shard_slugs.is_empty() || !cells.is_empty();

    RunPlan {
        shards: shard_slugs,
        cells,
        build_debug,
        build_dbi,
        build_aux,
        total_shards,
        total_cells,
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

fn fail(msg: &str) -> ! {
    eprintln!("select-tests: {msg}");
    std::process::exit(2);
}

fn changed_files_from_base(base: &str) -> Vec<String> {
    // Use merge-base so we compare against the fork point, matching how CI
    // evaluates a PR's own contribution.
    let out = Command::new("git")
        .args(["diff", "--name-only", &format!("{base}...HEAD")])
        .output()
        .unwrap_or_else(|e| fail(&format!("git diff failed: {e}")));
    if !out.status.success() {
        fail(&format!(
            "git diff against {base} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Union of two changed-file lists, de-duplicated and sorted. Pure so the
/// self-test can exercise the local-delta merge without touching git.
fn merge_delta(committed: Vec<String>, dirty: Vec<String>) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for f in committed.into_iter().chain(dirty.into_iter()) {
        let f = f.trim().to_string();
        if !f.is_empty() {
            set.insert(f);
        }
    }
    set.into_iter().collect()
}

fn git_lines(args: &[&str]) -> Vec<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .unwrap_or_else(|e| fail(&format!("git {:?} failed: {e}", args)));
    if !out.status.success() {
        fail(&format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// LOCAL delta relative to a known-green baseline commit:
///   committed since baseline  ∪  staged  ∪  unstaged  ∪  untracked.
/// This is the set of files whose green status is NOT vouched for by the
/// baseline, and therefore whose footprint must be re-tested locally.
fn local_changed_files(baseline: &str) -> Vec<String> {
    let committed = git_lines(&["diff", "--name-only", &format!("{baseline}...HEAD")]);
    let mut dirty = git_lines(&["diff", "--name-only", "HEAD"]); // staged + unstaged tracked
    // Untracked, respecting .gitignore.
    dirty.extend(git_lines(&["ls-files", "--others", "--exclude-standard"]));
    merge_delta(committed, dirty)
}

/// Resolve the last-known-green baseline commit for LOCAL selection.
///
/// Contract with `validate-run-ledger` (237b): the ledger owns the authoritative
/// "last commit whose validate run was green in this slot" record. This tool
/// stays storage-agnostic — the caller (a validate.sh wrapper) resolves the SHA
/// from the ledger and passes it via `--baseline`, or exports it as
/// `HERMIT_LAST_GREEN_SHA`. If neither is present, there is NO trustworthy
/// baseline, so selection MUST fall back to the full suite (never skip on an
/// unproven baseline).
fn resolve_baseline(explicit: &Option<String>) -> Option<String> {
    if let Some(b) = explicit {
        return Some(b.clone());
    }
    match std::env::var("HERMIT_LAST_GREEN_SHA") {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn read_files_stdin() -> Vec<String> {
    use std::io::Read;
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).ok();
    s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

fn repo_root() -> std::path::PathBuf {
    // This script lives at <repo>/ci/select-tests.rs.
    let here = std::env::current_exe().ok();
    let _ = here;
    // Prefer git toplevel; fall back to CWD.
    if let Ok(out) = Command::new("git").args(["rev-parse", "--show-toplevel"]).output() {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return std::path::PathBuf::from(p);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn main() {
    rust_script_prelude::init();
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return;
    }
    if args.iter().any(|a| a == "--self-test") {
        self_test();
        return;
    }

    let mut base: Option<String> = None;
    let mut format = "human".to_string();
    let mut explicit_files: Option<Vec<String>> = None;
    let mut since_green = false;
    let mut baseline: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--base" => {
                base = Some(args.get(i + 1).cloned().unwrap_or_else(|| fail("--base needs a ref")));
                i += 2;
            }
            "--baseline" => {
                baseline = Some(args.get(i + 1).cloned().unwrap_or_else(|| fail("--baseline needs a SHA")));
                i += 2;
            }
            "--since-green" => {
                since_green = true;
                i += 1;
            }
            "--format" => {
                format = args.get(i + 1).cloned().unwrap_or_else(|| fail("--format needs a value"));
                i += 2;
            }
            "--files" => {
                // `--files -` reads stdin. Otherwise collect following tokens as
                // paths, stopping at the next `--flag` so option order is free.
                if args.get(i + 1).map(|s| s.as_str()) == Some("-") {
                    explicit_files = Some(read_files_stdin());
                    i += 2;
                } else {
                    let mut files = Vec::new();
                    let mut j = i + 1;
                    while j < args.len() && !args[j].starts_with("--") {
                        files.push(args[j].clone());
                        j += 1;
                    }
                    explicit_files = Some(files);
                    i = j;
                }
            }
            other => fail(&format!("unknown argument: {other} (see --help)")),
        }
    }

    let root = repo_root();
    let fp = Footprints::load(&root.join("ci/test-footprints.json"));
    let dag = Dag::load(&root.join("ci/dag/portable.json"));
    let shards = Shards::load(&root.join("ci/portable-shards.json"));
    let plan = Plan::load(&root.join("ci/expected-e2e-plan.json"));

    // LOCAL mode: delta vs the last-known-green baseline (dirty tree + commits
    // since baseline). With NO trustworthy baseline we must run everything.
    if since_green {
        match resolve_baseline(&baseline) {
            Some(b) => {
                let files = local_changed_files(&b);
                let mut sel = select(&fp, &dag, &files);
                sel.reasons.insert(0, format!("LOCAL delta vs known-green baseline {b}"));
                let rp = derive_run_plan(&sel, &shards, &plan);
                emit(&sel, &rp, &format, files.len());
                return;
            }
            None => {
                let sel = Selection {
                    decision: Decision::Full,
                    nodes: dag.all_nodes.clone(),
                    e2e_all: true,
                    e2e_backends: BTreeSet::new(),
                    reasons: vec![
                        "no known-green baseline (pass --baseline <sha> or set \
                         HERMIT_LAST_GREEN_SHA from the validate-run-ledger) → full suite"
                            .into(),
                    ],
                };
                let rp = derive_run_plan(&sel, &shards, &plan);
                emit(&sel, &rp, &format, 0);
                return;
            }
        }
    }

    let files = match (explicit_files, base) {
        (Some(f), _) => f,
        (None, Some(b)) => changed_files_from_base(&b),
        (None, None) => fail("need --base <ref>, --files <paths…>, --files -, or --since-green"),
    };

    let sel = select(&fp, &dag, &files);
    let rp = derive_run_plan(&sel, &shards, &plan);
    emit(&sel, &rp, &format, files.len());
}

fn decision_str(d: &Decision) -> &'static str {
    match d {
        Decision::Skip => "skip",
        Decision::Selective => "selective",
        Decision::Full => "full",
    }
}

/// Build the GitHub Actions matrix objects the workflow consumes via fromJSON:
/// a shard-slug list and an e2e cell include-list (category/mode/backend/slug).
fn matrices(rp: &RunPlan) -> (Value, Value) {
    let shard_matrix = serde_json::json!({ "shards": rp.shards });
    let cell_matrix = serde_json::json!({
        "include": rp.cells.iter().map(|c| serde_json::json!({
            "category": c.category,
            "mode": c.mode,
            "backend": c.backend,
            "slug": Plan::slug(c),
        })).collect::<Vec<_>>()
    });
    (shard_matrix, cell_matrix)
}

fn emit(sel: &Selection, rp: &RunPlan, format: &str, n_files: usize) {
    match format {
        "human" => {
            println!("decision: {}", decision_str(&sel.decision));
            println!("changed files: {n_files}");
            for r in &sel.reasons {
                println!("  - {r}");
            }
            println!(
                "shards: {}/{}   e2e cells: {}/{}   build[debug={} dbi={} aux={}]",
                rp.shards.len(),
                rp.total_shards,
                rp.cells.len(),
                rp.total_cells,
                rp.build_debug,
                rp.build_dbi,
                rp.build_aux,
            );
            if !rp.shards.is_empty() {
                println!("  test shards: {}", rp.shards.join(", "));
            }
            if !rp.cells.is_empty() {
                let by_backend = {
                    let mut m: BTreeMap<String, usize> = BTreeMap::new();
                    for c in &rp.cells {
                        *m.entry(c.backend.clone()).or_default() += 1;
                    }
                    m.iter().map(|(b, n)| format!("{b}:{n}")).collect::<Vec<_>>().join(" ")
                };
                println!("  e2e cells by backend: {by_backend}");
            }
            if !sel.nodes.is_empty() {
                println!("nodes ({}):", sel.nodes.len());
                for n in &sel.nodes {
                    println!("  {n}");
                }
            }
        }
        "json" => {
            let (shard_matrix, cell_matrix) = matrices(rp);
            let obj = serde_json::json!({
                "decision": decision_str(&sel.decision),
                "skip": sel.decision == Decision::Skip,
                "full": sel.decision == Decision::Full,
                "node_count": sel.nodes.len(),
                "nodes": sel.nodes.iter().cloned().collect::<Vec<_>>(),
                "shards": rp.shards,
                "shard_matrix": shard_matrix,
                "cell_matrix": cell_matrix,
                "cell_count": rp.cells.len(),
                "build": { "debug": rp.build_debug, "dbi": rp.build_dbi, "aux": rp.build_aux },
                "reasons": sel.reasons,
            });
            println!("{}", serde_json::to_string_pretty(&obj).unwrap());
        }
        "github" => {
            let (shard_matrix, cell_matrix) = matrices(rp);
            let lines = format!(
                "decision={}\nskip={}\nfull={}\nnode_count={}\nnodes={}\n\
                 shard_count={}\ncell_count={}\nbuild_debug={}\nbuild_dbi={}\nbuild_aux={}\n\
                 shard_matrix={}\ncell_matrix={}\n",
                decision_str(&sel.decision),
                sel.decision == Decision::Skip,
                sel.decision == Decision::Full,
                sel.nodes.len(),
                sel.nodes.iter().cloned().collect::<Vec<_>>().join(" "),
                rp.shards.len(),
                rp.cells.len(),
                rp.build_debug,
                rp.build_dbi,
                rp.build_aux,
                serde_json::to_string(&shard_matrix).unwrap(),
                serde_json::to_string(&cell_matrix).unwrap(),
            );
            match std::env::var("GITHUB_OUTPUT") {
                Ok(path) => {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .unwrap_or_else(|e| fail(&format!("cannot open GITHUB_OUTPUT {path}: {e}")));
                    f.write_all(lines.as_bytes()).ok();
                }
                Err(_) => print!("{lines}"),
            }
            // Always echo a one-line summary to stderr for the CI log.
            eprintln!(
                "select-tests: decision={} nodes={} shards={} cells={}",
                decision_str(&sel.decision),
                sel.nodes.len(),
                rp.shards.len(),
                rp.cells.len(),
            );
        }
        other => fail(&format!("unknown --format {other} (human|json|github)")),
    }
}

fn print_help() {
    print!(
        "\
Usage: ci/select-tests.rs [--base <ref> | --since-green | --files <paths…> | --files -]
                          [--baseline <sha>] [--format human|json|github]
       ci/select-tests.rs --self-test

Decide which portable-DAG nodes a commit's changed files can affect.

  --base <ref>     GITHUB mode: diff <ref>...HEAD (merge-base) — the PR's own
                   contribution vs the green target branch.
  --since-green    LOCAL mode: delta vs the last-known-green baseline commit,
                   i.e. commits-since-baseline ∪ staged ∪ unstaged ∪ untracked.
                   The baseline comes from --baseline or $HERMIT_LAST_GREEN_SHA
                   (the validate-run-ledger's last-green SHA for this slot). With
                   NO baseline, selection falls back to the full suite.
  --baseline <sha> Known-green baseline commit for --since-green.
  --files <paths>  Use an explicit path list (space-separated).
  --files -        Read the path list from stdin (one per line).
  --format         human (default), json, or github ($GITHUB_OUTPUT).
  --self-test      Run built-in unit tests and exit non-zero on failure.

Selection is only SOUND when the baseline it trusts is actually green: it runs
the tests the delta can affect and trusts the baseline for the rest. Outcomes:
skip (all files inert) | selective (subset + deps) | full (forced/unknown paths,
or no trustworthy baseline ⇒ conservative). See ci/test-footprints.json.

The selected node set is then projected onto how CI actually runs post-44df2944
(see ci/portable-shards.json + ci/expected-e2e-plan.json):
  * test shards  — a shard runs iff ANY of its nodes was selected.
  * e2e cells    — the (category × mode × backend) matrix, filtered by per-change
                   BACKEND AFFINITY: a detcore-dbi change runs only dbi cells, a
                   detcore-sabre change only sabre cells, a core/CLI/fixture
                   change runs every cell. (Portable lane has no KVM cells.)
  * release builds — build-dbi / build-aux are emitted only when a selected shard
                   or cell needs them.
--format github writes shard_matrix + cell_matrix (GitHub-Actions fromJSON-ready)
plus shard_count/cell_count/build_* to $GITHUB_OUTPUT.
"
    );
}

// ---------------------------------------------------------------------------
// Built-in tests
// ---------------------------------------------------------------------------

fn self_test() {
    let mut failures = 0;
    let mut total = 0;
    let mut check = |name: &str, cond: bool| {
        total += 1;
        if cond {
            println!("ok   - {name}");
        } else {
            println!("FAIL - {name}");
            failures += 1;
        }
    };

    // --- glob matcher ---
    check("glob exact", glob_match("Cargo.toml", "Cargo.toml"));
    check("glob exact not suffix", !glob_match("Cargo.toml", "detcore/Cargo.toml"));
    check("glob star segment", glob_match("*.md", "README.md"));
    check("glob star no cross slash", !glob_match("*.md", "docs/x.md"));
    check("glob doublestar suffix", glob_match("docs/**", "docs/a/b/c.md"));
    check("glob doublestar rs", glob_match("**/*.rs", "detcore/src/scheduler.rs"));
    check("glob doublestar rs root", glob_match("**/*.rs", "main.rs"));
    check("glob dir rs", glob_match("detcore/**/*.rs", "detcore/src/a/b.rs"));
    check("glob dir rs miss", !glob_match("detcore/**/*.rs", "hermit-cli/src/a.rs"));
    check("glob prefix dir", glob_match("detcore-dbi/**", "detcore-dbi/src/lib.rs"));
    check("glob prefix dir file", glob_match("detcore-dbi/**", "detcore-dbi/Cargo.toml"));
    check("glob question", glob_match("a?c", "abc"));
    check("glob mid doublestar", glob_match("a/**/z.c", "a/b/c/z.c"));
    check("glob mid doublestar empty", glob_match("a/**/z.c", "a/z.c"));

    // --- selection scenarios ---
    let root = repo_root();
    let fp = Footprints::load(&root.join("ci/test-footprints.json"));
    let dag = Dag::load(&root.join("ci/dag/portable.json"));
    let shards = Shards::load(&root.join("ci/portable-shards.json"));
    let plan = Plan::load(&root.join("ci/expected-e2e-plan.json"));

    let docs = select(&fp, &dag, &vec!["ai_docs/x.md".into(), "docs/y.md".into(), "README.md".into()]);
    check("docs-only ⇒ skip", docs.decision == Decision::Skip && docs.nodes.is_empty());

    let lock = select(&fp, &dag, &vec!["Cargo.lock".into()]);
    check("Cargo.lock ⇒ full", lock.decision == Decision::Full && lock.nodes.len() == dag.all_nodes.len());

    let toolchain = select(&fp, &dag, &vec!["rust-toolchain.toml".into()]);
    check("toolchain ⇒ full", toolchain.decision == Decision::Full);

    let ci = select(&fp, &dag, &vec!["ci/dag/portable.json".into()]);
    check("ci/** ⇒ full", ci.decision == Decision::Full);

    let dbi = select(&fp, &dag, &vec!["detcore-dbi/src/lib.rs".into()]);
    check("dbi-only ⇒ selective", dbi.decision == Decision::Selective);
    check("dbi-only runs dbi_parity", dbi.nodes.contains("test.dbi_parity"));
    check("dbi-only pulls build.runtime_release", dbi.nodes.contains("build.runtime_release"));
    check("dbi-only pulls build.workspace (dep)", dbi.nodes.contains("build.workspace"));
    check("dbi-only skips strict_compat", !dbi.nodes.contains("test.strict_compat"));
    check("dbi-only skips language_runtimes", !dbi.nodes.contains("e2e.manifest_language_runtimes"));
    check("dbi-only is a strict subset", dbi.nodes.len() < dag.all_nodes.len());
    check("dbi-only includes preflight", dbi.nodes.contains("lint.rustfmt"));

    let core = select(&fp, &dag, &vec!["detcore/src/scheduler.rs".into()]);
    check("detcore core ⇒ selective", core.decision == Decision::Selective);
    check("detcore core runs strict_compat", core.nodes.contains("test.strict_compat"));
    check("detcore core runs detcore_unit", core.nodes.contains("test.detcore_unit"));

    let unknown = select(&fp, &dag, &vec!["some/brand/new/area/file.py".into()]);
    check("unknown path ⇒ full", unknown.decision == Decision::Full);

    let mixed = select(&fp, &dag, &vec!["detcore-dbi/src/lib.rs".into(), "README.md".into()]);
    check("dbi + docs ⇒ selective (docs inert)", mixed.decision == Decision::Selective);

    let mixed2 = select(&fp, &dag, &vec!["detcore-dbi/src/lib.rs".into(), "Cargo.lock".into()]);
    check("dbi + Cargo.lock ⇒ full (force wins)", mixed2.decision == Decision::Full);

    let rs_lint = select(&fp, &dag, &vec!["hermit-verify/src/main.rs".into()]);
    check("rs change pulls clippy", rs_lint.nodes.contains("lint.clippy"));

    // --- shard + e2e cell derivation (footprint → shard-selection layer) ---
    let total_cells = plan.cells.len();
    let total_shards = shards.debug.len() + shards.release.len();

    let rp_docs = derive_run_plan(&docs, &shards, &plan);
    check("docs ⇒ 0 shards", rp_docs.shards.is_empty());
    check("docs ⇒ 0 cells", rp_docs.cells.is_empty());
    check("docs ⇒ no debug build", !rp_docs.build_debug);

    let rp_full = derive_run_plan(&lock, &shards, &plan);
    check("full ⇒ all shards", rp_full.shards.len() == total_shards);
    check("full ⇒ all cells", rp_full.cells.len() == total_cells);
    check("full ⇒ all builds", rp_full.build_debug && rp_full.build_dbi && rp_full.build_aux);

    // DBI is a Cargo dependency of hermit. Package-level reverse-dependency
    // closure therefore includes Hermit's other third-party-backend test
    // nodes, while explicit backend affinity still limits e2e cells to DBI.
    let rp_dbi = derive_run_plan(&dbi, &shards, &plan);
    check("dbi ⇒ dbi-parity shard", rp_dbi.shards.contains(&"dbi-parity".to_string()));
    check("dbi ⇒ hermit reverse-dep sabre shard", rp_dbi.shards.contains(&"sabre".to_string()));
    check("dbi ⇒ only dbi cells", !rp_dbi.cells.is_empty() && rp_dbi.cells.iter().all(|c| c.backend == "dbi"));
    check("dbi ⇒ reverse-dep builds dbi and aux", rp_dbi.build_dbi && rp_dbi.build_aux);
    check("dbi ⇒ cells are a strict subset", rp_dbi.cells.len() < total_cells);

    // SaBRe backend change: only sabre cells + sabre shard.
    let sabre = select(&fp, &dag, &vec!["detcore-sabre/src/lib.rs".into()]);
    let rp_sabre = derive_run_plan(&sabre, &shards, &plan);
    check("sabre ⇒ sabre shard", rp_sabre.shards.contains(&"sabre".to_string()));
    check("sabre ⇒ only sabre cells", !rp_sabre.cells.is_empty() && rp_sabre.cells.iter().all(|c| c.backend == "sabre"));
    check("sabre ⇒ build_dbi, not aux", rp_sabre.build_dbi && !rp_sabre.build_aux);

    // LiteInst runtime change: only liteinst cells + liteinst shard.
    let liteinst = select(&fp, &dag, &vec!["scripts/stage-liteinst-runtime.sh".into()]);
    let rp_lite = derive_run_plan(&liteinst, &shards, &plan);
    check("liteinst ⇒ liteinst shard", rp_lite.shards.contains(&"liteinst".to_string()));
    check("liteinst ⇒ only liteinst cells", !rp_lite.cells.is_empty() && rp_lite.cells.iter().all(|c| c.backend == "liteinst"));

    // Core change: all backends' cells (shared Detcore path).
    let rp_core = derive_run_plan(&core, &shards, &plan);
    check("core ⇒ all e2e cells", rp_core.cells.len() == total_cells);
    check("core ⇒ e2e_all set", core.e2e_all);

    // Pure standalone-script change: shards but no e2e cells.
    let rp_scripts = derive_run_plan(&rs_lint, &shards, &plan);
    check("hermit-verify ⇒ 0 e2e cells", rp_scripts.cells.is_empty());

    // Backend disjointness: the dbi and sabre cell sets never overlap.
    check(
        "dbi vs sabre cells disjoint",
        !rp_dbi.cells.iter().any(|d| rp_sabre.cells.iter().any(|s| Plan::slug(d) == Plan::slug(s))),
    );

    // --- local-delta merge (pure) ---
    let merged = merge_delta(
        vec!["a.rs".into(), "b.rs".into(), " ".into()],
        vec!["b.rs".into(), "c.rs".into(), "".into()],
    );
    check("merge_delta unions", merged == vec!["a.rs", "b.rs", "c.rs"]);
    check(
        "merge_delta trims blanks",
        !merged.iter().any(|s| s.trim().is_empty()),
    );

    // --- baseline resolution (explicit wins; env fallback is a runtime concern) ---
    check(
        "resolve_baseline explicit",
        resolve_baseline(&Some("deadbeef".into())) == Some("deadbeef".into()),
    );

    // A resolved baseline feeds the SAME select() path as a PR diff, so the
    // local delta of a docs-only change must still skip.
    let local_docs = select(&fp, &dag, &vec!["docs/z.md".into()]);
    check("local docs-only ⇒ skip", local_docs.decision == Decision::Skip);

    drop(check);
    println!("\n{total} check(s), {failures} failure(s)");
    if failures > 0 {
        std::process::exit(1);
    }
}
