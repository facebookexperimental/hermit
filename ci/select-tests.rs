#!/usr/bin/env -S rust-script --force
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
//!                   run preflight only (the PREFLIGHT checks plus lint).
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
//! `ci/dag/validate.json` (single source of truth); the path→node relation is
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

use std::collections::BTreeMap;
use std::collections::BTreeSet;
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

/// One coarse backend-affinity rule compiled from the policy. `paths` are
/// Hermit-tree globs; `reverie_paths` are globs relative to the reverie
/// submodule root (matched against `reverie/`-prefixed changed files after a
/// pin bump is expanded). `tier` is `backend` (scoped) or `core` (full suite).
struct BackendRule {
    id: String,
    paths: Vec<String>,
    reverie_paths: Vec<String>,
    backends: Vec<String>,
    lanes: Vec<String>,
    extra_nodes: Vec<String>,
    tier: String,
}

struct Footprints {
    groups: BTreeMap<String, Vec<String>>,
    force_full: Vec<String>,
    ci_irrelevant: Vec<String>,
    footprints: Vec<Fp>,
    backend_rules: Vec<BackendRule>,
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

        let backend_rules = v["backend_rules"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|rule| BackendRule {
                        id: rule["id"].as_str().unwrap_or("").to_string(),
                        paths: str_vec(&rule["paths"]),
                        reverie_paths: str_vec(&rule["reverie_paths"]),
                        backends: str_vec(&rule["backends"]),
                        lanes: str_vec(&rule["lanes"]),
                        extra_nodes: str_vec(&rule["extra_nodes"]),
                        tier: rule["tier"].as_str().unwrap_or("backend").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Footprints {
            groups,
            force_full,
            ci_irrelevant,
            footprints,
            backend_rules,
        }
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
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
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
            if !str_vec(&s["labels"])
                .iter()
                .any(|label| label == "hosted-portable")
            {
                continue;
            }
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

    fn resolve_selection_node(&self, node: &str) -> Option<&str> {
        self.all_nodes
            .get(node)
            .or_else(|| self.all_nodes.get(&format!("{node}_on_host")))
            .map(String::as_str)
    }

    fn expand_selection_aliases(&self, nodes: &mut BTreeSet<String>) {
        *nodes = nodes
            .iter()
            .map(|node| {
                self.resolve_selection_node(node)
                    .unwrap_or(node)
                    .to_string()
            })
            .collect();
    }
}

// Always-on cheap safety gates for any selective run.
const PREFLIGHT: &[&str] = &[
    "check.skill_discovery",
    "check.backend_abstraction",
    "check.portability_paths",
    // Unconditional on purpose: this node guards the merge-gate status
    // classifier, so a selective run that skipped it would leave the gate that
    // decides landing unguarded on exactly the changes least likely to look
    // related to it. It is 5s and classified light.
    "check.check_outcome_consumers",
    "check.script_sigpipe",
    "lint.rustfmt",
];

// ---------------------------------------------------------------------------
// Shard model (ci/portable-shards.json) + e2e cell plan (expected-e2e-plan.json)
// ---------------------------------------------------------------------------

/// One test shard: a workflow matrix cell running a set of DAG nodes.
/// `needs` is the release-build dependency ("dbt", "aux") or empty for debug.
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
        Shards {
            debug: parse("debug_shards"),
            release: parse("release_shards"),
        }
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
    /// Backend-rule ids matched by the change (empty for non-backend changes).
    backend_rules: Vec<String>,
    /// Lanes declared by the matched backend rules.
    backend_lanes: Vec<String>,
    reasons: Vec<String>,
}

fn matches_any(globs: &[String], file: &str) -> bool {
    globs.iter().any(|g| glob_match(g, file))
}

/// Whether `file` matches a backend rule, either as a Hermit-tree path or as
/// a `reverie/`-prefixed path from an expanded pin bump.
fn backend_rule_matches(rule: &BackendRule, file: &str) -> bool {
    if matches_any(&rule.paths, file) {
        return true;
    }
    match file.strip_prefix("reverie/") {
        Some(inner) => matches_any(&rule.reverie_paths, inner),
        None => false,
    }
}

fn select(fp: &Footprints, dag: &Dag, files: &[String]) -> Selection {
    let mut reasons = Vec::new();

    // Full-suite result: every node, and every e2e cell (e2e_all).
    let full = |nodes: BTreeSet<String>, reasons: Vec<String>| Selection {
        decision: Decision::Full,
        nodes,
        e2e_all: true,
        e2e_backends: BTreeSet::new(),
        backend_rules: Vec::new(),
        backend_lanes: Vec::new(),
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
    let mut matched_rules: Vec<String> = Vec::new();
    let mut matched_lanes: Vec<String> = Vec::new();
    let mut core_rule: Option<String> = None;
    // DBT-only requires EVERY non-inert changed path to qualify as DBT.
    let mut every_noninert_dbt = true;

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
        // Coarse backend rules (data-driven from the compiled table). A
        // backend-tier hit scopes E2E cells to that rule's backends only —
        // no implicit ptrace addition — while a core-tier hit (shared
        // Reverie crates) forces the full suite below.
        for rule in &fp.backend_rules {
            if backend_rule_matches(rule, f) {
                if !matched_rules.contains(&rule.id) {
                    matched_rules.push(rule.id.clone());
                }
                for lane in &rule.lanes {
                    if !matched_lanes.contains(lane) {
                        matched_lanes.push(lane.clone());
                    }
                }
                if rule.tier == "core" {
                    core_rule = Some(rule.id.clone());
                } else {
                    let mut seen = BTreeSet::new();
                    fp.expand(&rule.extra_nodes, &mut matched, &mut seen);
                    for b in &rule.backends {
                        if b != "*" {
                            e2e_backends.insert(b.clone());
                        }
                    }
                }
                hit = true;
            }
        }
        if hit {
            all_inert = false;
            let dbt_qualifying = fp
                .backend_rules
                .iter()
                .filter(|rule| rule.id == "dbt")
                .any(|rule| backend_rule_matches(rule, f))
                || fp.footprints.iter().any(|entry| {
                    matches_any(&entry.paths, f)
                        && !entry.e2e_all
                        && entry.e2e_backends.iter().any(|b| b == "dbt")
                });
            if !dbt_qualifying {
                every_noninert_dbt = false;
            }
            continue;
        }
        if matches_any(&fp.ci_irrelevant, f) {
            continue; // inert; leaves all_inert as-is
        }
        // Matched no rule at all → unknown → conservative full.
        unknown.push(f.clone());
        all_inert = false;
    }

    if let Some(rule_id) = &core_rule {
        reasons.push(format!("shared core rule `{rule_id}` matched → full suite"));
        let mut selection = full(dag.all_nodes.clone(), reasons);
        selection.backend_rules = matched_rules;
        selection.backend_lanes = matched_lanes;
        return selection;
    }
    if force {
        return full(dag.all_nodes.clone(), reasons);
    }
    if !unknown.is_empty() {
        reasons.push(format!(
            "{} unmapped path(s) (e.g. {}) → conservative full suite",
            unknown.len(),
            unknown
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
        return full(dag.all_nodes.clone(), reasons);
    }
    if matched.is_empty() && all_inert {
        reasons.push("all changed files are CI-irrelevant → skip to preflight only".into());
        // Inert/docs selections still execute the PREFLIGHT check nodes
        // plus both lint nodes (lint.rustfmt is already in PREFLIGHT).
        let mut nodes: BTreeSet<String> = BTreeSet::new();
        for pf in PREFLIGHT {
            if dag.all_nodes.contains(*pf) {
                nodes.insert((*pf).to_string());
            }
        }
        for lint in ["lint.clippy_on_host", "lint.clippy"] {
            if dag.all_nodes.contains(lint) {
                nodes.insert(lint.to_string());
                break;
            }
        }
        return Selection {
            decision: Decision::Skip,
            nodes,
            e2e_all: false,
            e2e_backends: BTreeSet::new(),
            backend_rules: matched_rules,
            backend_lanes: matched_lanes,
            reasons,
        };
    }
    if matched.is_empty() {
        // Defensive: no match, not proven inert. Fail safe. A backend-rule
        // hit with no nodes is NOT exempt: without a build/test/e2e node
        // nothing exercises the backend, so it falls back to everything.
        reasons.push("no footprint matched but files not proven inert → full suite".into());
        return full(dag.all_nodes.clone(), reasons);
    }

    // v1 scope cut: the only scoped (selective) route is DBT-only. Docs-only
    // changes already returned Skip above; every other selective candidate
    // (shared/core code, another backend, a mixed change, a standalone
    // check) runs the full label. Residual future work: scoped kvm, ptrace,
    // liteinst/in-guest-trap, sabre and e9patch-preprocessing routes, with
    // buckets derived from their selected cells and, for Reverie crates,
    // the union of their direct Hermit dependents' footprints.
    let dbt_only =
        matched_rules.len() == 1 && matched_rules[0] == "dbt" && !e2e_all && every_noninert_dbt;
    if !dbt_only {
        reasons.push(
            "v1 selective scope is DBT-only; this change is not confined to DBT paths → full suite"
                .into(),
        );
        return full(dag.all_nodes.clone(), reasons);
    }

    dag.expand_selection_aliases(&mut matched);

    // A backend-rule hit must exercise its backend: without any build,
    // test or e2e node in the rule/footprint set itself (dependency
    // closure pulling a build node does not count), fall back to
    // everything.
    if !matched_rules.is_empty()
        && !matched.iter().any(|node| {
            node.starts_with("build.") || node.starts_with("test.") || node.starts_with("e2e.")
        })
    {
        reasons.push(
            "backend rule matched but selected no build/test/e2e node → full suite".into(),
        );
        return full(dag.all_nodes.clone(), reasons);
    }

    // Selective: add preflight, then close over build deps.
    for pf in PREFLIGHT {
        if dag.all_nodes.contains(*pf) {
            matched.insert((*pf).to_string());
        }
    }
    // Drop any footprint node that is not in the current DAG (schema drift guard).
    let unknown_nodes: Vec<String> = matched
        .iter()
        .filter(|n| !dag.all_nodes.contains(*n))
        .cloned()
        .collect();
    if !unknown_nodes.is_empty() {
        reasons.push(format!(
            "footprint referenced {} node(s) absent from validate.json ({}) → full suite (stale map)",
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
    if !matched_rules.is_empty() {
        reasons.push(format!(
            "backend rules matched → {}",
            matched_rules.join(", ")
        ));
    }
    Selection {
        decision: Decision::Selective,
        nodes: closed,
        e2e_all,
        e2e_backends,
        backend_rules: matched_rules,
        backend_lanes: matched_lanes,
        reasons,
    }
}

// ---------------------------------------------------------------------------
// Shard + cell derivation (footprint → shard-selection layer)
// ---------------------------------------------------------------------------

/// A concrete run plan: which test shards and which e2e cells to execute, plus
/// the release-build jobs those selections require.
struct RunPlan {
    shards: Vec<String>, // shard slugs (debug + release) to run
    cells: Vec<Cell>,    // e2e cells to run
    build_debug: bool,   // the shared debug build job
    build_dbt: bool,     // the DBT release artifact
    build_aux: bool,     // the sabre/liteinst release artifacts
    total_shards: usize, // universe sizes, for reporting
    total_cells: usize,
}

/// Resolve public shard selectors against the same hosted node universe as
/// footprint selectors. A missing node must never silently omit a shard.
fn resolved_shard_nodes(shard: &Shard, dag: &Dag) -> Result<BTreeSet<String>, String> {
    shard
        .nodes
        .iter()
        .map(|node| {
            dag.resolve_selection_node(node)
                .map(str::to_owned)
                .ok_or_else(|| {
                    format!("shard {} references unknown hosted node {node}", shard.slug)
                })
        })
        .collect()
}

/// Map a node-level Selection onto shards and e2e cells.
///
/// A test shard runs iff any of its nodes is in the selected set. An e2e cell
/// runs iff the change is e2e_all, or the cell's backend is in the selection's
/// backend set. Build jobs are pulled in only when a selected shard/cell needs
/// them. `full` runs everything; `skip` runs nothing.
fn derive_run_plan(sel: &Selection, shards: &Shards, plan: &Plan, dag: &Dag) -> RunPlan {
    let total_shards = shards.debug.len() + shards.release.len();
    let total_cells = plan.cells.len();

    let (shard_slugs, cells): (Vec<String>, Vec<Cell>) = match sel.decision {
        Decision::Skip => (Vec::new(), Vec::new()),
        Decision::Full => (
            shards
                .debug
                .iter()
                .chain(&shards.release)
                .map(|s| s.slug.clone())
                .collect(),
            plan.cells.clone(),
        ),
        Decision::Selective => {
            let runs = |s: &Shard| {
                let nodes = resolved_shard_nodes(s, dag).unwrap_or_else(|error| fail(&error));
                !nodes.is_disjoint(&sel.nodes)
            };
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
    let mut build_dbt = false;
    let mut build_aux = false;
    for s in &shards.release {
        if shard_slugs.contains(&s.slug) {
            match s.needs.as_str() {
                "dbt" => build_dbt = true,
                "aux" => build_aux = true,
                _ => {}
            }
        }
    }
    for c in &cells {
        match c.backend.as_str() {
            "dbt" | "sabre" => build_dbt = true,
            // in-guest-trap runs the same in-guest LiteInst runtime.
            "liteinst" | "in-guest-trap" => build_aux = true,
            _ => {}
        }
    }
    // Anything running at all needs the shared debug build.
    let build_debug = !shard_slugs.is_empty() || !cells.is_empty();

    RunPlan {
        shards: shard_slugs,
        cells,
        build_debug,
        build_dbt,
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

/// Parse `git diff --name-status` output, counting BOTH sides of a
/// rename/copy (a rename's source can be a core path even when its
/// destination is backend-scoped). Bare path lines pass through.
fn parse_name_status(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let Some(first) = fields.next() else { continue };
        let status = first.trim();
        if status.starts_with('R') || status.starts_with('C') {
            for field in fields {
                let field = field.trim();
                if !field.is_empty() {
                    out.push(field.to_string());
                }
            }
        } else if let Some(path) = fields.next().or(Some(status)) {
            // --name-status line: path is field 2; bare line: path is field 1.
            let path = path.trim();
            if !path.is_empty() {
                out.push(path.to_string());
            }
        }
    }
    out
}

fn changed_files_from_base(base: &str) -> Vec<String> {
    // Use merge-base so we compare against the fork point, matching how CI
    // evaluates a PR's own contribution.
    let out = Command::new("git")
        .args(["diff", "--name-status", "-M", &format!("{base}...HEAD")])
        .output()
        .unwrap_or_else(|e| fail(&format!("git diff failed: {e}")));
    if !out.status.success() {
        fail(&format!(
            "git diff against {base} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    parse_name_status(&String::from_utf8_lossy(&out.stdout))
}

/// Union of two changed-file lists, de-duplicated and sorted. Pure so the
/// self-test can exercise the local-delta merge without touching git.
fn merge_delta(committed: Vec<String>, dirty: Vec<String>) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for f in committed.into_iter().chain(dirty) {
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
fn git_name_status(args: &[&str]) -> Vec<String> {
    parse_name_status(&git_lines(args).join("\n"))
}

fn local_changed_files(baseline: &str) -> Vec<String> {
    let committed = git_name_status(&["diff", "--name-status", "-M", &format!("{baseline}...HEAD")]);
    let mut dirty = git_name_status(&["diff", "--name-status", "-M", "HEAD"]); // staged + unstaged tracked
    // Untracked, respecting .gitignore.
    dirty.extend(git_lines(&["ls-files", "--others", "--exclude-standard"]));
    merge_delta(committed, dirty)
}

/// Resolve the last-known-green baseline commit for LOCAL selection.
///
/// Contract with `validate-run-ledger` (237b): the ledger owns the authoritative
/// "last commit whose validate run was green in this slot" record. This tool
/// stays storage-agnostic — the caller (a scripts/validate.rs wrapper) resolves the SHA
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

/// Run a git command in `root`; return trimmed stdout on success.
fn git_text(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Expand a bare `reverie` gitlink entry into the submodule's inner changed
/// files (`reverie/<inner path>`), diffing the old and new pins. Any failure
/// leaves the bare entry in place, which matches no rule and therefore
/// forces the full suite (fail-safe).
fn expand_reverie_pin(root: &Path, files: Vec<String>, base: Option<&str>) -> Vec<String> {
    if !files.iter().any(|f| f == "reverie") {
        return files;
    }
    let expanded = (|| -> Option<Vec<String>> {
        let old = match base {
            Some(b) => {
                let mb = git_text(root, &["merge-base", b, "HEAD"])?;
                git_text(root, &["rev-parse", &format!("{mb}:reverie")])?
            }
            None => git_text(root, &["rev-parse", "HEAD:reverie"])?,
        };
        let new = git_text(&root.join("reverie"), &["rev-parse", "HEAD"])
            .or_else(|| git_text(root, &["rev-parse", "HEAD:reverie"]))?;
        let mut inner_paths: Vec<String> = Vec::new();
        if old != new {
            let diff = git_text(
                &root.join("reverie"),
                &["diff", "--name-status", "-M", &old, &new],
            )?;
            inner_paths.extend(parse_name_status(&diff));
            // Union dirty reverie edits (tracked + untracked) when the pin
            // moved: uncommitted work inside the submodule is real delta too.
            if let Some(dirty) = git_text(
                &root.join("reverie"),
                &["diff", "--name-status", "-M", "HEAD"],
            ) {
                inner_paths.extend(parse_name_status(&dirty));
            }
            if let Some(untracked) = git_text(
                &root.join("reverie"),
                &["ls-files", "--others", "--exclude-standard"],
            ) {
                inner_paths.extend(
                    untracked.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string),
                );
            }
        } else {
            // Uncommitted submodule move: diff the submodule worktree.
            let diff = git_text(
                &root.join("reverie"),
                &["diff", "--name-status", "-M", "HEAD"],
            )
            .unwrap_or_default();
            inner_paths.extend(parse_name_status(&diff));
            if let Some(untracked) = git_text(
                &root.join("reverie"),
                &["ls-files", "--others", "--exclude-standard"],
            ) {
                inner_paths.extend(
                    untracked.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string),
                );
            }
        }
        let inner: Vec<String> = inner_paths
            .iter()
            .map(|line| format!("reverie/{line}"))
            .filter(|line| line != "reverie/")
            .collect();
        if inner.is_empty() {
            return None;
        }
        let mut out: Vec<String> = files
            .iter()
            .filter(|f| f.as_str() != "reverie")
            .cloned()
            .collect();
        out.extend(inner);
        out.sort();
        out.dedup();
        Some(out)
    })();
    expanded.unwrap_or(files)
}

fn read_files_stdin() -> Vec<String> {
    use std::io::Read;
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).ok();
    s.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn repo_root() -> std::path::PathBuf {
    // This script lives at <repo>/ci/select-tests.rs.
    let here = std::env::current_exe().ok();
    let _ = here;
    // Prefer git toplevel; fall back to CWD.
    if let Ok(out) = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
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
                base = Some(
                    args.get(i + 1)
                        .cloned()
                        .unwrap_or_else(|| fail("--base needs a ref")),
                );
                i += 2;
            }
            "--baseline" => {
                baseline = Some(
                    args.get(i + 1)
                        .cloned()
                        .unwrap_or_else(|| fail("--baseline needs a SHA")),
                );
                i += 2;
            }
            "--since-green" => {
                since_green = true;
                i += 1;
            }
            "--format" => {
                format = args
                    .get(i + 1)
                    .cloned()
                    .unwrap_or_else(|| fail("--format needs a value"));
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
    let dag = Dag::load(&root.join("ci/dag/validate.json"));
    let shards = Shards::load(&root.join("ci/portable-shards.json"));
    let plan = Plan::load(&root.join("ci/expected-e2e-plan.json"));

    // LOCAL mode: delta vs the last-known-green baseline (dirty tree + commits
    // since baseline). With NO trustworthy baseline we must run everything.
    if since_green {
        match resolve_baseline(&baseline) {
            Some(b) => {
                let files = expand_reverie_pin(&root, local_changed_files(&b), Some(&b));
                let mut sel = select(&fp, &dag, &files);
                sel.reasons
                    .insert(0, format!("LOCAL delta vs known-green baseline {b}"));
                let rp = derive_run_plan(&sel, &shards, &plan, &dag);
                emit(&sel, &rp, &format, files.len());
                return;
            }
            None => {
                let sel = Selection {
                    decision: Decision::Full,
                    nodes: dag.all_nodes.clone(),
                    e2e_all: true,
                    e2e_backends: BTreeSet::new(),
                    backend_rules: Vec::new(),
                    backend_lanes: Vec::new(),
                    reasons: vec![
                        "no known-green baseline (pass --baseline <sha> or set \
                         HERMIT_LAST_GREEN_SHA from the validate-run-ledger) → full suite"
                            .into(),
                    ],
                };
                let rp = derive_run_plan(&sel, &shards, &plan, &dag);
                emit(&sel, &rp, &format, 0);
                return;
            }
        }
    }

    let files = match (explicit_files, base.clone()) {
        (Some(f), _) => expand_reverie_pin(&root, f, base.as_deref()),
        (None, Some(b)) => {
            let files = changed_files_from_base(&b);
            expand_reverie_pin(&root, files, Some(&b))
        }
        (None, None) => fail("need --base <ref>, --files <paths…>, --files -, or --since-green"),
    };

    let sel = select(&fp, &dag, &files);
    let rp = derive_run_plan(&sel, &shards, &plan, &dag);
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
                "shards: {}/{}   e2e cells: {}/{}   build[debug={} dbt={} aux={}]",
                rp.shards.len(),
                rp.total_shards,
                rp.cells.len(),
                rp.total_cells,
                rp.build_debug,
                rp.build_dbt,
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
                    m.iter()
                        .map(|(b, n)| format!("{b}:{n}"))
                        .collect::<Vec<_>>()
                        .join(" ")
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
            // Selected backends: every backend present in the selected cells
            // for all-backend/full runs, else the scoped backend set.
            let backends: Vec<String> = if sel.e2e_all || sel.decision == Decision::Full {
                let mut set = BTreeSet::new();
                for c in &rp.cells {
                    set.insert(c.backend.clone());
                }
                set.into_iter().collect()
            } else {
                sel.e2e_backends.iter().cloned().collect()
            };
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
                "cells": rp.cells.iter().map(Plan::slug).collect::<Vec<_>>(),
                "backends": backends,
                "e2e_all": sel.e2e_all,
                "backend_rules": sel.backend_rules,
                "backend_lanes": sel.backend_lanes,
                "build": { "debug": rp.build_debug, "dbt": rp.build_dbt, "aux": rp.build_aux },
                "reasons": sel.reasons,
            });
            println!("{}", serde_json::to_string_pretty(&obj).unwrap());
        }
        "github" => {
            let (shard_matrix, cell_matrix) = matrices(rp);
            let backends: Vec<String> = if sel.e2e_all || sel.decision == Decision::Full {
                let mut set = BTreeSet::new();
                for c in &rp.cells {
                    set.insert(c.backend.clone());
                }
                set.into_iter().collect()
            } else {
                sel.e2e_backends.iter().cloned().collect()
            };
            let lines = format!(
                "decision={}\nskip={}\nfull={}\nnode_count={}\nnodes={}\n\
                 shard_count={}\ncell_count={}\nbuild_debug={}\nbuild_dbt={}\nbuild_aux={}\n\
                 backends={}\nbackend_rules={}\nshard_matrix={}\ncell_matrix={}\n",
                decision_str(&sel.decision),
                sel.decision == Decision::Skip,
                sel.decision == Decision::Full,
                sel.nodes.len(),
                sel.nodes.iter().cloned().collect::<Vec<_>>().join(" "),
                rp.shards.len(),
                rp.cells.len(),
                rp.build_debug,
                rp.build_dbt,
                rp.build_aux,
                backends.join(","),
                sel.backend_rules.join(","),
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
                        .unwrap_or_else(|e| {
                            fail(&format!("cannot open GITHUB_OUTPUT {path}: {e}"))
                        });
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
                   BACKEND AFFINITY: a detcore-dbt change runs only dbt cells, a
                   detcore-sabre change only sabre cells, a core/CLI/fixture
                   change runs every cell. (Portable lane has no KVM cells.)
  * release builds — build-dbt / build-aux are emitted only when a selected shard
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
    check(
        "glob exact not suffix",
        !glob_match("Cargo.toml", "detcore/Cargo.toml"),
    );
    check("glob star segment", glob_match("*.md", "README.md"));
    check("glob star no cross slash", !glob_match("*.md", "docs/x.md"));
    check(
        "glob doublestar suffix",
        glob_match("docs/**", "docs/a/b/c.md"),
    );
    check(
        "glob doublestar rs",
        glob_match("**/*.rs", "detcore/src/scheduler.rs"),
    );
    check("glob doublestar rs root", glob_match("**/*.rs", "main.rs"));
    check(
        "glob dir rs",
        glob_match("detcore/**/*.rs", "detcore/src/a/b.rs"),
    );
    check(
        "glob dir rs miss",
        !glob_match("detcore/**/*.rs", "hermit-cli/src/a.rs"),
    );
    check(
        "glob prefix dir",
        glob_match("detcore-dbt/**", "detcore-dbt/src/lib.rs"),
    );
    check(
        "glob prefix dir file",
        glob_match("detcore-dbt/**", "detcore-dbt/Cargo.toml"),
    );
    check("glob question", glob_match("a?c", "abc"));
    check("glob mid doublestar", glob_match("a/**/z.c", "a/b/c/z.c"));
    check("glob mid doublestar empty", glob_match("a/**/z.c", "a/z.c"));

    // --- selection scenarios ---
    let root = repo_root();
    let fp = Footprints::load(&root.join("ci/test-footprints.json"));
    let dag = Dag::load(&root.join("ci/dag/validate.json"));
    let shards = Shards::load(&root.join("ci/portable-shards.json"));
    let plan = Plan::load(&root.join("ci/expected-e2e-plan.json"));

    check(
        "every declared shard node resolves in the actual hosted DAG",
        shards
            .debug
            .iter()
            .chain(&shards.release)
            .all(|shard| resolved_shard_nodes(shard, &dag).is_ok()),
    );
    let alias_fixture = Dag {
        all_nodes: ["test.exact", "test.exact_on_host", "test.public_on_host"]
            .into_iter()
            .map(String::from)
            .collect(),
        deps: BTreeMap::new(),
    };
    check(
        "public selector resolves only its exact hosted counterpart",
        alias_fixture.resolve_selection_node("test.public") == Some("test.public_on_host"),
    );
    check(
        "literal node wins over a simultaneous hosted counterpart",
        alias_fixture.resolve_selection_node("test.exact") == Some("test.exact"),
    );
    check(
        "unknown and partial selector names do not resolve",
        alias_fixture.resolve_selection_node("test.pub").is_none()
            && alias_fixture
                .resolve_selection_node("test.public_typo")
                .is_none(),
    );

    let docs = select(
        &fp,
        &dag,
        &[
            "ai_docs/x.md".into(),
            "docs/y.md".into(),
            "README.md".into(),
        ],
    );
    check("docs-only ⇒ skip", docs.decision == Decision::Skip);
    // v3 counterexample (inert-without-preflight): a docs/inert selection
    // must still execute the PREFLIGHT check nodes plus both lint nodes.
    check(
        "v3 inert docs-only retains every PREFLIGHT node",
        docs.decision == Decision::Skip && PREFLIGHT.iter().all(|node| docs.nodes.contains(*node)),
    );
    check(
        "v3 inert docs-only retains lint.rustfmt and lint.clippy",
        docs.nodes.contains("lint.rustfmt") && docs.nodes.contains("lint.clippy_on_host"),
    );

    let lock = select(&fp, &dag, &["Cargo.lock".into()]);
    check(
        "Cargo.lock ⇒ full",
        lock.decision == Decision::Full && lock.nodes.len() == dag.all_nodes.len(),
    );

    let toolchain = select(&fp, &dag, &["rust-toolchain.toml".into()]);
    check("toolchain ⇒ full", toolchain.decision == Decision::Full);

    let ci = select(&fp, &dag, &["ci/dag/validate.json".into()]);
    check("ci/** ⇒ full", ci.decision == Decision::Full);

    let skill = select(&fp, &dag, &[".claude/skills/benchmark/SKILL.md".into()]);
    check(
        "skill change ⇒ full (v1: only DBT-only is selective)",
        skill.decision == Decision::Full,
    );
    check(
        "skill change runs discovery check",
        skill.nodes.contains("check.skill_discovery"),
    );
    check(
        "skill change runs rust-script SIGPIPE/cache guard",
        skill.nodes.contains("check.script_sigpipe"),
    );

    let dbt = select(&fp, &dag, &["detcore-dbt/src/lib.rs".into()]);
    check("dbt-only ⇒ selective", dbt.decision == Decision::Selective);
    check(
        "dbt-only runs the DBT runtime ABI check",
        dbt.nodes.contains("check.dbt_runtime_abi_on_host"),
    );
    check(
        "dbt-only runs c-programs DBT cells",
        dbt.nodes.contains("e2e.manifest_c_programs_on_host"),
    );
    check(
        "dbt-only runs system-utils DBT cells",
        dbt.nodes.contains("e2e.manifest_system_utils_on_host"),
    );
    check(
        "dbt-only pulls the E2E publisher (the one validate-profile build since 2026-09-30)",
        dbt.nodes.contains("build.e2e_artifact_on_host"),
    );
    check(
        "dbt-only pulls build.workspace (dep)",
        dbt.nodes.contains("build.workspace_on_host"),
    );
    check(
        "dbt-only skips compat (no DBT cells in the current plan)",
        !dbt.nodes.contains("e2e.manifest_compat_on_host"),
    );
    check(
        "dbt-only runs language_runtimes (has DBT cells)",
        dbt.nodes.contains("e2e.manifest_language_runtimes_on_host"),
    );
    // DBT buckets == buckets containing a DBT cell, derived from the plan.
    {
        let expected: std::collections::BTreeSet<String> = plan
            .cells
            .iter()
            .filter(|c| c.backend == "dbt")
            .map(|c| format!("e2e.manifest_{}_on_host", c.category.replace('-', "_")))
            .collect();
        let actual: std::collections::BTreeSet<String> = dbt
            .nodes
            .iter()
            .filter(|n| n.starts_with("e2e.manifest_"))
            .cloned()
            .collect();
        check(
            "dbt buckets == buckets containing DBT cells (derived)",
            !expected.is_empty() && actual == expected,
        );
    }
    check(
        "dbt-only is a strict subset",
        dbt.nodes.len() < dag.all_nodes.len(),
    );
    check(
        "dbt-only includes preflight",
        dbt.nodes.contains("lint.rustfmt"),
    );

    let core = select(&fp, &dag, &["detcore/src/scheduler.rs".into()]);
    check(
        "detcore core ⇒ full (v1: shared code runs everything)",
        core.decision == Decision::Full,
    );
    check(
        "detcore core runs the strict compatibility corpus",
        core.nodes.contains("e2e.manifest_compat_on_host"),
    );
    check(
        "detcore core runs detcore_unit",
        core.nodes.contains("test.detcore_unit_on_host"),
    );

    let mut removed = Dag {
        all_nodes: dag.all_nodes.clone(),
        deps: dag.deps.clone(),
    };
    check(
        "actual hosted DBT counterpart is present before removal",
        removed
            .all_nodes
            .remove("e2e.manifest_system_utils_on_host"),
    );
    check(
        "removed footprint counterpart forces full instead of dropping its coverage",
        select(&fp, &removed, &["detcore-dbt/src/lib.rs".into()]).decision == Decision::Full,
    );
    let mut removed = Dag {
        all_nodes: dag.all_nodes.clone(),
        deps: dag.deps.clone(),
    };
    check(
        "actual hosted strict compatibility bucket is present before removal",
        removed.all_nodes.remove("e2e.manifest_compat_on_host"),
    );
    check(
        "a removed strict compatibility bucket forces full",
        select(&fp, &removed, &["detcore/src/scheduler.rs".into()]).decision == Decision::Full,
    );
    let unknown_fp = Footprints {
        groups: BTreeMap::new(),
        force_full: vec![],
        ci_irrelevant: vec![],
        backend_rules: vec![],
        footprints: vec![Fp {
            paths: vec!["fixture/**".into()],
            nodes: vec!["test.public_typo".into()],
            e2e_all: false,
            e2e_backends: vec![],
        }],
    };
    check(
        "unknown footprint selector forces full",
        select(&unknown_fp, &alias_fixture, &["fixture/input".into()]).decision == Decision::Full,
    );

    let workdir = select(&fp, &dag, &["common/test-workdir/src/lib.rs".into()]);
    check(
        "shared workdir ⇒ full, incl. its application consumer",
        workdir.decision == Decision::Full
            && workdir.nodes.contains("test.applications_e2e_on_host"),
    );
    check(
        "shared workdir retains all-backend E2E affinity",
        workdir.e2e_all,
    );

    let unknown = select(&fp, &dag, &["some/brand/new/area/file.py".into()]);
    check("unknown path ⇒ full", unknown.decision == Decision::Full);

    let mixed = select(
        &fp,
        &dag,
        &["detcore-dbt/src/lib.rs".into(), "README.md".into()],
    );
    check(
        "dbt + docs ⇒ selective (docs inert)",
        mixed.decision == Decision::Selective,
    );

    // v3 counterexample (mixed DBT+other selecting DBT-only):
    // detcore-dbt/src/lib.rs plus hermit-verify/src/main.rs must run the
    // full label, because DBT-only requires EVERY non-inert changed path
    // to qualify as DBT.
    let mixed_dbt_other = select(
        &fp,
        &dag,
        &[
            "detcore-dbt/src/lib.rs".into(),
            "hermit-verify/src/main.rs".into(),
        ],
    );
    check(
        "v3 mixed DBT + non-DBT non-inert ⇒ full",
        mixed_dbt_other.decision == Decision::Full,
    );

    let mixed2 = select(
        &fp,
        &dag,
        &["detcore-dbt/src/lib.rs".into(), "Cargo.lock".into()],
    );
    check(
        "dbt + Cargo.lock ⇒ full (force wins)",
        mixed2.decision == Decision::Full,
    );

    let rs_lint = select(&fp, &dag, &["hermit-verify/src/main.rs".into()]);
    check(
        "rs change pulls clippy",
        rs_lint.nodes.contains("lint.clippy_on_host"),
    );

    // --- shard + e2e cell derivation (footprint → shard-selection layer) ---
    let total_cells = plan.cells.len();
    let total_shards = shards.debug.len() + shards.release.len();

    let rp_docs = derive_run_plan(&docs, &shards, &plan, &dag);
    check("docs ⇒ 0 shards", rp_docs.shards.is_empty());
    check("docs ⇒ 0 cells", rp_docs.cells.is_empty());
    check("docs ⇒ no debug build", !rp_docs.build_debug);

    let rp_full = derive_run_plan(&lock, &shards, &plan, &dag);
    check("full ⇒ all shards", rp_full.shards.len() == total_shards);
    check("full ⇒ all cells", rp_full.cells.len() == total_cells);
    // No release shard needs the aux build since test.liteinst_strict and its
    // liteinst shard were retired (https://github.com/rrnewton/hermit/issues/3520),
    // so a full run builds aux exactly when the plan has a liteinst cell.
    let full_needs_aux = plan.cells.iter().any(|c| c.backend == "liteinst")
        || shards.release.iter().any(|s| s.needs == "aux");
    check(
        "full ⇒ all builds",
        rp_full.build_debug && rp_full.build_dbt && rp_full.build_aux == full_needs_aux,
    );
    // Positive control independent of the committed cell population: a
    // planned liteinst cell must still request the aux build.
    let liteinst_cell_fixture = Plan {
        cells: vec![Cell {
            category: "fixture-l".into(),
            mode: "run".into(),
            backend: "liteinst".into(),
        }],
    };
    check(
        "full with a liteinst cell ⇒ aux build",
        derive_run_plan(&lock, &shards, &liteinst_cell_fixture, &dag).build_aux,
    );

    // DBT is a Cargo dependency of hermit. Package-level reverse-dependency
    // closure therefore includes Hermit's other third-party-backend test
    // nodes, while explicit backend affinity still limits e2e cells to DBT.
    let rp_dbt = derive_run_plan(&dbt, &shards, &plan, &dag);
    check(
        "dbt ⇒ dbt-runtime-abi shard",
        rp_dbt.shards.contains(&"dbt-runtime-abi".to_string()),
    );
    check(
        "dbt ⇒ hermit reverse-dep sabre shard",
        rp_dbt.shards.contains(&"sabre".to_string()),
    );
    let expected_dbt_cells = plan
        .cells
        .iter()
        .filter(|cell| cell.backend == "dbt")
        .count();
    check(
        "dbt ⇒ exactly the planned dbt cells",
        rp_dbt.cells.len() == expected_dbt_cells
            && rp_dbt.cells.iter().all(|cell| cell.backend == "dbt"),
    );
    // The committed plan currently has no DBT cells. Keep a positive control
    // independent of that population so dropping every DBT cell cannot pass.
    let mixed_backend_fixture = Plan {
        cells: vec![
            Cell {
                category: "fixture-a".into(),
                mode: "run".into(),
                backend: "dbt".into(),
            },
            Cell {
                category: "fixture-a".into(),
                mode: "run".into(),
                backend: "ptrace".into(),
            },
            Cell {
                category: "fixture-b".into(),
                mode: "verify".into(),
                backend: "dbt".into(),
            },
            Cell {
                category: "fixture-b".into(),
                mode: "verify".into(),
                backend: "sabre".into(),
            },
        ],
    };
    let selected_fixture = derive_run_plan(&dbt, &shards, &mixed_backend_fixture, &dag);
    check(
        "dbt fixture ⇒ both DBT identities and no other backend",
        selected_fixture
            .cells
            .iter()
            .map(Plan::slug)
            .collect::<Vec<_>>()
            == vec![
                "fixture-a__run__dbt".to_string(),
                "fixture-b__verify__dbt".to_string(),
            ],
    );
    // aux is no longer a reverse dependency of DBT: its one release shard
    // (liteinst) was retired with test.liteinst_strict (https://github.com/rrnewton/hermit/issues/3520).
    check(
        "dbt ⇒ reverse-dep builds dbt, not aux",
        rp_dbt.build_dbt && !rp_dbt.build_aux,
    );
    check(
        "dbt ⇒ cells are a strict subset",
        rp_dbt.cells.len() < total_cells,
    );

    // SaBRe backend change: only sabre cells + sabre shard. SaBRe is archived
    // (ci/manifest-plan/src/archived.rs), so the committed plan has no SaBRe
    // cell and the change selects none of its cells; the fixture plan keeps
    // the backend affinity positive.
    let sabre = select(&fp, &dag, &["detcore-sabre/src/lib.rs".into()]);
    let rp_sabre = derive_run_plan(&sabre, &shards, &plan, &dag);
    check(
        "sabre ⇒ full (v1: dropped route runs everything)",
        sabre.decision == Decision::Full && rp_sabre.cells.len() == total_cells,
    );

    // LiteInst runtime staging: hermit-install's build script runs this
    // script for every Hermit build, and no node is its own consumer since
    // test.liteinst_strict was retired (https://github.com/rrnewton/hermit/issues/3520),
    // so the unmatched path must force the complete suite.
    let liteinst = select(&fp, &dag, &["scripts/stage-liteinst-runtime.sh".into()]);
    check(
        "liteinst runtime staging ⇒ full",
        liteinst.decision == Decision::Full,
    );

    // Core change: all backends' cells (shared Detcore path).
    let rp_core = derive_run_plan(&core, &shards, &plan, &dag);
    check("core ⇒ all e2e cells", rp_core.cells.len() == total_cells);
    check("core ⇒ e2e_all set", core.e2e_all);

    // Pure standalone-script change: shards but no e2e cells.
    let rp_scripts = derive_run_plan(&rs_lint, &shards, &plan, &dag);
    check(
        "hermit-verify ⇒ full e2e cells (v1: everything else is full)",
        rp_scripts.cells.len() == total_cells,
    );
    check(
        "hermit-verify retains the regular-crates unit-parallel shard",
        rp_scripts.shards.contains(&"unit-parallel".to_string()),
    );

    let only_regular = Selection {
        decision: Decision::Selective,
        nodes: BTreeSet::from(["test.regular_crates_on_host".into()]),
        e2e_all: false,
        e2e_backends: BTreeSet::new(),
        backend_rules: Vec::new(),
        backend_lanes: Vec::new(),
        reasons: vec![],
    };
    let regular_plan = derive_run_plan(&only_regular, &shards, &plan, &dag);
    check(
        "one hosted regular node selects exactly its original public shard",
        regular_plan.shards == vec!["unit-parallel".to_string()]
            && regular_plan.cells.is_empty()
            && regular_plan.build_debug
            && !regular_plan.build_dbt
            && !regular_plan.build_aux,
    );
    let mut missing = Dag {
        all_nodes: dag.all_nodes.clone(),
        deps: dag.deps.clone(),
    };
    check(
        "actual regular counterpart exists before the missing-shard control",
        missing.all_nodes.remove("test.regular_crates_on_host"),
    );
    let regular_shard = shards
        .debug
        .iter()
        .find(|shard| shard.slug == "unit-parallel")
        .unwrap();
    check(
        "removed declared shard counterpart is refused",
        resolved_shard_nodes(regular_shard, &missing).is_err(),
    );
    let literal_shard = Shard {
        slug: "literal".into(),
        needs: String::new(),
        nodes: vec!["test.exact".into()],
    };
    check(
        "shard resolution also prefers the literal node over another hosted node",
        resolved_shard_nodes(&literal_shard, &alias_fixture).unwrap()
            == BTreeSet::from(["test.exact".into()]),
    );

    check(
        "dbt cells are a strict subset of the full (sabre-route) set",
        rp_dbt.cells.len() < rp_sabre.cells.len()
            && rp_dbt
                .cells
                .iter()
                .all(|d| rp_sabre.cells.iter().any(|s| Plan::slug(d) == Plan::slug(s))),
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
    let local_docs = select(&fp, &dag, &["docs/z.md".into()]);
    check(
        "local docs-only ⇒ skip",
        local_docs.decision == Decision::Skip,
    );

    // --- coarse backend rules ---
    let kvm = select(&fp, &dag, &["hermit-run-kvm/src/lib.rs".into()]);
    let rp_kvm = derive_run_plan(&kvm, &shards, &plan, &dag);
    let expected_kvm_cells = plan
        .cells
        .iter()
        .filter(|cell| cell.backend == "kvm")
        .count();
    check(
        "kvm-only ⇒ full (v1: dropped route runs everything)",
        kvm.decision == Decision::Full && rp_kvm.cells.len() == total_cells,
    );
    check(
        "kvm cells exist in the full population",
        expected_kvm_cells > 0,
    );
    check(
        "dbt-only ⇒ dbt cells and not kvm cells",
        rp_dbt.cells.iter().all(|cell| cell.backend == "dbt")
            && !rp_dbt.cells.iter().any(|cell| cell.backend == "kvm")
            && dbt.e2e_backends == BTreeSet::from(["dbt".to_string()]),
    );
    let ptrace = select(&fp, &dag, &["hermit-run-ptrace/src/lib.rs".into()]);
    let rp_ptrace = derive_run_plan(&ptrace, &shards, &plan, &dag);
    check(
        "ptrace-only ⇒ full (v1: dropped route runs everything)",
        ptrace.decision == Decision::Full && rp_ptrace.cells.len() == total_cells,
    );
    check(
        "detcore change ⇒ every backend's cells",
        core.e2e_all && rp_core.cells.len() == total_cells,
    );
    // Reverie pin-bump expansion feeds `reverie/`-prefixed inner paths.
    let rev_dbt = select(&fp, &dag, &["reverie/reverie-dbt/src/lib.rs".into()]);
    let rp_rev_dbt = derive_run_plan(&rev_dbt, &shards, &mixed_backend_fixture, &dag);
    check(
        "reverie dbt crate ⇒ selective dbt-only cells",
        rev_dbt.decision == Decision::Selective
            && rev_dbt.e2e_backends == BTreeSet::from(["dbt".to_string()])
            && rp_rev_dbt.cells.iter().map(Plan::slug).collect::<Vec<_>>()
                == vec![
                    "fixture-a__run__dbt".to_string(),
                    "fixture-b__verify__dbt".to_string(),
                ],
    );
    let rev_kvm = select(&fp, &dag, &["reverie/reverie-kvm-core/src/lib.rs".into()]);
    check(
        "reverie kvm crate ⇒ full (v1: dropped route)",
        rev_kvm.decision == Decision::Full,
    );
    let rev_core = select(&fp, &dag, &["reverie/reverie/src/lib.rs".into()]);
    check(
        "reverie core crate ⇒ full suite",
        rev_core.decision == Decision::Full,
    );
    let rev_bare = select(&fp, &dag, &["reverie".into()]);
    check(
        "unexpanded reverie gitlink ⇒ full suite",
        rev_bare.decision == Decision::Full,
    );
    let e9 = select(&fp, &dag, &["reverie/reverie-e9patch/src/lib.rs".into()]);
    check(
        "reverie e9patch ⇒ full (preprocessing route dropped in v1)",
        e9.decision == Decision::Full,
    );

    // Backend rules must select real execution work (build + test/e2e).
    check(
        "reverie-kvm ⇒ build.workspace + a test/e2e node",
        rev_kvm.nodes.contains("build.workspace_on_host")
            && rev_kvm
                .nodes
                .iter()
                .any(|n| n.starts_with("test.") || n.starts_with("e2e.")),
    );
    check(
        "hermit-run-kvm only ⇒ e2e buckets",
        kvm.nodes.iter().any(|n| n.starts_with("e2e.manifest")),
    );
    check(
        "hermit-run-ptrace only ⇒ e2e buckets",
        ptrace.nodes.iter().any(|n| n.starts_with("e2e.manifest")),
    );
    check(
        "safeptrace-only ⇒ everything (multi-backend crate is core)",
        select(&fp, &dag, &["reverie/safeptrace/src/lib.rs".into()]).decision
            == Decision::Full,
    );
    check(
        "reverie-inguest-only ⇒ everything (multi-backend crate is core)",
        select(&fp, &dag, &["reverie/reverie-inguest/src/lib.rs".into()]).decision
            == Decision::Full,
    );
    // Rename: both sides counted; core source side forces everything.
    check(
        "parse_name_status counts both rename sides",
        parse_name_status("R100\treverie/reverie-syscalls/src/x.rs\treverie/reverie-kvm/src/x.rs\nM\tdetcore/src/y.rs\n")
            == vec![
                "reverie/reverie-syscalls/src/x.rs".to_string(),
                "reverie/reverie-kvm/src/x.rs".to_string(),
                "detcore/src/y.rs".to_string(),
            ],
    );
    check(
        "rename syscalls→kvm (both sides) ⇒ everything",
        select(
            &fp,
            &dag,
            &[
                "reverie/reverie-syscalls/src/x.rs".into(),
                "reverie/reverie-kvm/src/x.rs".into(),
            ],
        )
        .decision
            == Decision::Full,
    );
    // A backend rule with no build/test/e2e node falls back to everything.
    let weak_fp = Footprints {
        groups: BTreeMap::new(),
        force_full: vec![],
        ci_irrelevant: vec![],
        footprints: vec![],
        backend_rules: vec![BackendRule {
            id: "weak".into(),
            paths: vec!["weak-backend/**".into()],
            reverie_paths: vec![],
            backends: vec!["kvm".into()],
            lanes: vec!["portable".into()],
            extra_nodes: vec!["check.skill_discovery".into()],
            tier: "backend".into(),
        }],
    };
    check(
        "backend hit with no build/test/e2e node ⇒ full",
        select(&weak_fp, &dag, &["weak-backend/src/lib.rs".into()]).decision
            == Decision::Full,
    );

    println!("\n{total} check(s), {failures} failure(s)");
    if failures > 0 {
        std::process::exit(1);
    }
}
