// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Two readers of the validate-run ledger: the TREE-KEYED result cache
//! (`cache_lookup_record`, validate.sh:620) and the runtime ESTIMATE
//! (`history_estimate`, validate.sh:936).
//!
//! # The cache predicate must match the PRODUCER and its evidence
//!
//! Both producers require `executed_tests > 0`: a missing count and a measured
//! zero are distinct facts, but neither proves that a test-bearing run completed.
//! The Rust driver carries that count from typed step outcomes, independently of
//! human-facing log verbosity. It also writes `executed_nodes`, because a
//! ~47-NODE DAG run must never be readable as a 47-TEST pass (see `write_ledger`
//! in `validate.rs`).
//!
//! So the predicate is dispatched on the row's own `producer` field: a
//! `validate.rs` row must carry `executed_tests > 0`, `executed_nodes > 0`,
//! **and** a satisfied coverage record; a bash row must carry
//! `executed_tests > 0`. That is one verifier per authority rather than one
//! generic field test, and a Rust row cannot substitute its node count for test
//! evidence.
//!
//! # Fail-open, never fail-hit
//!
//! Every unreadable/absent/ambiguous condition yields "no hit" and a real run.
//! A missing ledger, a malformed line, an unknown producer, an absent coverage
//! block — none of them can manufacture a reuse.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// A qualifying prior run, with enough context that the printed banner cannot
/// misdescribe what was reused.
#[derive(Clone, Debug)]
pub struct CacheHit {
    pub finished_at: String,
    pub real_seconds: f64,
    pub cpu_seconds: f64,
    /// The count the producer actually recorded.
    pub executed: i64,
    /// What that count COUNTS. Never collapsed: "test(s)" for a bash row,
    /// "node(s)" for a validate.rs row.
    pub executed_unit: &'static str,
    /// Raw typed counts retained for a current validation-service result.
    /// These remain optional because historical cache rows did not carry them.
    pub schema_version: Option<i64>,
    pub executed_nodes: Option<i64>,
    pub executed_tests: Option<i64>,
    pub passed_tests: Option<i64>,
    pub commit: String,
    pub producer: String,
}

impl CacheHit {
    /// Counts that can support a current successful validation-service result.
    ///
    /// A cache hit may still serve the historical CLI cache contract without
    /// these values. Publishing a new schema result is stricter: only the
    /// current typed ledger schema, written by the Rust producer, can supply
    /// positive executed-node evidence and exact equal test counts.
    pub fn exact_current_pass_counts(&self) -> Option<(i64, i64, i64)> {
        if self.schema_version
            != Some(if crate::validate_evidence::ENABLED { 10 } else { crate::validate_cell_results::CELL_RESULTS_LEDGER_SCHEMA_VERSION })
            || !matches!(
                self.producer.as_str(),
                "validate.rs" | "hermit-validate-rs"
            )
        {
            return None;
        }
        let nodes = self.executed_nodes?;
        let executed = self.executed_tests?;
        let passed = self.passed_tests?;
        (nodes > 0 && executed > 0 && passed == executed)
            .then_some((nodes, executed, passed))
    }
}

/// The adapter executable comes from the immutable tooling checkout, while
/// `ledger` continues to name canonical shared state. Older direct callers
/// without a tool root retain the historical parent-relative behavior.
pub fn canonical_ledger_adapter(ledger: &Path, tool_root: Option<&Path>) -> Option<std::path::PathBuf> {
    let state_root = ledger.parent()?;
    Some(
        tool_root
            .unwrap_or(state_root)
            .join("ci-hub/ledger/validate_rows.py"),
    )
}

/// Read the one logical ledger into rows, skipping unparseable lines.
///
/// In an admitted dev-hermit run, `ledger` is the parent's logical `ledger/`
/// root.  The parent adapter owns sharding and union semantics; opening one
/// shard (or the retired raw shadow file) here would create a second receipt
/// authority.  An ordinary file remains supported only for isolated fixtures
/// and genuinely standalone checkouts.
pub fn read_rows(ledger: &Path) -> Vec<serde_json::Value> {
    let explicit = std::env::var("HERMIT_VALIDATE_LEDGER")
        .ok()
        .filter(|value| !value.is_empty())
        .is_some_and(|value| Path::new(&value) == ledger);
    let text = if !explicit && ledger.file_name().is_some_and(|name| name == "ledger") {
        let configured_tool_root = std::env::var_os("DEV_HERMIT_TOOL_ROOT")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from);
        let Some(adapter) = canonical_ledger_adapter(ledger, configured_tool_root.as_deref()) else {
            return Vec::new();
        };
        let Ok(output) = Command::new("python3").arg(&adapter).arg("rows").output() else {
            eprintln!(
                "validate: warning: cannot launch canonical ledger reader {}",
                adapter.display()
            );
            return Vec::new();
        };
        if !output.status.success() {
            eprintln!(
                "validate: warning: canonical ledger reader {} refused: {}",
                adapter.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return Vec::new();
        }
        let Ok(text) = String::from_utf8(output.stdout) else {
            eprintln!("validate: warning: canonical ledger reader emitted non-UTF-8 data");
            return Vec::new();
        };
        text
    } else {
        let Ok(text) = std::fs::read_to_string(ledger) else { return Vec::new() };
        text
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

fn s<'a>(row: &'a serde_json::Value, k: &str) -> &'a str {
    row.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn i(row: &serde_json::Value, k: &str) -> Option<i64> {
    row.get(k).and_then(|v| v.as_i64())
}

fn f(row: &serde_json::Value, k: &str) -> f64 {
    row.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

/// Identity of the run whose result may be reused. Every field here is part of
/// the cache KEY, so a value that differs on any of them is a different run.
pub struct CacheKey<'a> {
    pub tree: &'a str,
    pub profile: &'a str,
    pub host: &'a str,
    pub toolchain: &'a str,
}

/// The gate-coverage half of the predicate, shared by both producers.
fn gate_coverage_ok(row: &serde_json::Value) -> bool {
    match (i(row, "gates_expected"), i(row, "gates_run")) {
        (None, _) => true, // gates_expected null: no obligation was recorded
        (Some(exp), Some(run)) => run >= exp,
        (Some(_), None) => false,
    }
}

fn row_matches_key(row: &serde_json::Value, key: &CacheKey<'_>) -> bool {
    s(row, "tree") == key.tree
        && s(row, "profile") == key.profile
        && s(row, "host") == key.host
        && s(row, "toolchain") == key.toolchain
        && s(row, "selection_mode") == "full"
        && row.get("commit_anchored").and_then(|v| v.as_bool()) == Some(true)
        && row.get("tree_dirty").and_then(|v| v.as_bool()) == Some(false)
}

/// A failure which carries enough execution evidence to latch this cache key.
/// Environment/no-result rows, contended runs and incomplete rows do not poison
/// reuse; they still cause an actual run through their ordinary result path.
fn failure_row_blocks_pass_cache(row: &serde_json::Value, key: &CacheKey<'_>) -> bool {
    if !row_matches_key(row, key) || !matches!(s(row, "result"), "fail" | "failed" | "timeout") {
        return false;
    }
    let Some(gates) = row.get("gates").and_then(|value| value.as_array()) else {
        return false;
    };
    let red_gates: Vec<_> = gates
        .iter()
        .filter(|gate| matches!(s(gate, "result"), "fail" | "failed" | "timeout"))
        .collect();
    if red_gates.is_empty() {
        return false;
    }
    let any_genuine_red = red_gates.iter().any(|gate| {
        i(gate, "exit_code") != Some(127)
            && gate
                .get("real_seconds")
                .and_then(|value| value.as_f64())
                .is_some_and(|seconds| seconds > 0.0)
    });
    let command_not_found_storm = red_gates
        .iter()
        .any(|gate| i(gate, "exit_code") == Some(127))
        && !any_genuine_red;
    let subsecond_collapse = row
        .get("real_seconds")
        .and_then(|value| value.as_f64())
        .is_some_and(|seconds| seconds <= 1.0)
        && gates
            .iter()
            .all(|gate| matches!(s(gate, "result"), "fail" | "failed" | "timeout"));
    if command_not_found_storm || subsecond_collapse {
        return false;
    }
    let Some(expected) = i(row, "gates_expected") else {
        return false;
    };
    let Some(ran) = i(row, "gates_run") else {
        return false;
    };
    if expected <= 0 || ran < expected {
        return false;
    }
    let has_real_failure =
        i(row, "failures").is_some_and(|failures| failures >= 1) || !red_gates.is_empty();
    let origin_bound = red_gates
        .iter()
        .all(|gate| match s(gate, "failure_origin") {
            "outer_gate" => true,
            "lane_substep" => gate
                .get("failed_substeps")
                .and_then(|value| value.as_array())
                .is_some_and(|substeps| !substeps.is_empty()),
            _ => false,
        });
    if !has_real_failure || !origin_bound {
        return false;
    }
    let jobs = i(row, "dag_jobs");
    let peers = i(row, "concurrent_validates");
    let conditions_are_solo = jobs.is_some_and(|value| value <= 4) && peers == Some(0);
    if !conditions_are_solo {
        return false;
    }
    // Read the outer version before interpreting the nested shape. A newer
    // schema may retain familiar field names with different meaning; accepting
    // one recognizable state before checking the version would grant an
    // unsupported row failure authority.
    let typed_cell_divergence = i(row, "schema_version").is_some_and(|schema| {
        (crate::validate_cell_results::CELL_RESULTS_LEDGER_SCHEMA_MIN
            ..=crate::validate_cell_results::CELL_RESULTS_LEDGER_SCHEMA_VERSION)
            .contains(&schema)
            && row
                .get("cell_results")
                .and_then(|value| value.get("cells"))
                .and_then(|value| value.as_array())
                .is_some_and(|cells| {
                    cells.iter().any(|cell| {
                        cell.get("cell_verdict")
                            .and_then(|verdict| verdict.get("state"))
                            .and_then(|state| state.as_str())
                            == Some("compared-and-diverged")
                    })
                })
    });
    // Schema 10 keeps reference and cross-backend outcomes separate. This
    // conservative cache refusal grants no qualification or failure-obligation
    // authority; those readers authenticate the complete retained artifacts.
    let parity_divergence = i(row, "schema_version") == Some(10)
        && row.get("cell_results").and_then(|v| v.get("cells"))
            .and_then(serde_json::Value::as_array).is_some_and(|cells| cells.iter().any(|cell| {
                cell.get("cell_verdict").and_then(|v| v.get("state"))
                    .and_then(serde_json::Value::as_str) == Some("compared-and-diverged")
                || cell.get("backend_parity").and_then(|v| v.get("attempts"))
                    .and_then(serde_json::Value::as_array).is_some_and(|attempts| attempts.iter().any(|attempt| {
                        ["candidate", "reference"].iter().any(|role| attempt.get(role)
                            .and_then(|v| v.get("state")).and_then(serde_json::Value::as_str)
                                == Some("compared-and-diverged"))
                        || attempt.get("cross").and_then(|v| v.get("state"))
                            .and_then(serde_json::Value::as_str) == Some("diverged")
                    }))
            }));
    if typed_cell_divergence || parity_divergence {
        return true;
    }
    let known_flaky = row.get("known_flaky_failure").and_then(|v| v.as_bool());
    let solo_confirmation = row.get("solo_rerun_confirmation").and_then(|v| v.as_bool());
    match known_flaky {
        Some(false) => true,
        Some(true) => solo_confirmation == Some(true),
        None => false,
    }
}

fn has_blocking_failure(rows: &[serde_json::Value], key: &CacheKey<'_>) -> bool {
    rows
        .iter()
        .any(|row| failure_row_blocks_pass_cache(row, key))
}

/// Does this PASS row carry everything a reuse needs?
///
/// Dispatched on `producer`; an unrecognized producer is REFUSED rather than
/// guessed at, so a future third writer cannot be silently cached under
/// whichever field name happens to be present.
fn pass_row_qualifies(row: &serde_json::Value) -> bool {
    if i(row, "failures") != Some(0) {
        return false;
    }
    if !gate_coverage_ok(row) {
        return false;
    }
    match s(row, "producer") {
        "validate.rs" | "hermit-validate-rs" => {
            if i(row, "executed_tests").unwrap_or(0) <= 0 {
                return false;
            }
            if i(row, "executed_nodes").unwrap_or(0) <= 0 {
                return false;
            }
            // Coverage is a first-class part of the claim: a run that PLANNED
            // test nodes and did not execute some of them is not a full pass, so
            // its result must not be reused as one.
            match row.get("coverage") {
                None => false,
                Some(c) => {
                    let absent = c.get("absent_nodes").and_then(|a| a.as_array()).map(|a| a.len());
                    let executed = c.get("executed_test_nodes").and_then(|v| v.as_i64());
                    matches!(absent, Some(0)) && executed.is_some()
                }
            }
        }
        // A validate.sh-era row has no typed node/coverage evidence, so its
        // positive test count is the producer-specific execution proof.
        "" | "validate.sh" => i(row, "executed_tests").unwrap_or(0) > 0,
        _ => false,
    }
}

/// Newest qualifying record for `want_result`, or `None`.
///
/// Port of `cache_lookup_record` (validate.sh:620) with the producer-aware
/// predicate described in the module doc.
pub fn cache_lookup(
    rows: &[serde_json::Value],
    want_result: &str,
    key: &CacheKey,
) -> Option<CacheHit> {
    if key.tree.is_empty() || key.tree == "unknown" {
        return None;
    }
    // A clean full failure for this exact tree/profile/host/toolchain is a
    // durable obligation. A sibling PASS cannot return a zero-gate cache hit;
    // validate must execute and ci-hub will require calibrated per-cell
    // requalification before the candidate can acquire landing authority.
    if want_result == "pass" && has_blocking_failure(rows, key) {
        return None;
    }
    let mut best: Option<&serde_json::Value> = None;
    for row in rows {
        if !row_matches_key(row, key) || s(row, "result") != want_result {
            continue;
        }
        if want_result == "pass" && !pass_row_qualifies(row) {
            continue;
        }
        let newer = match best {
            None => true,
            Some(b) => s(row, "finished_at") >= s(b, "finished_at"),
        };
        if newer {
            best = Some(row);
        }
    }
    let row = best?;
    let producer = s(row, "producer");
    let (executed, unit) = if matches!(producer, "validate.rs" | "hermit-validate-rs") {
        (i(row, "executed_nodes").unwrap_or(0), "node(s)")
    } else {
        (i(row, "executed_tests").unwrap_or(0), "test(s)")
    };
    Some(CacheHit {
        finished_at: s(row, "finished_at").to_string(),
        real_seconds: f(row, "real_seconds"),
        cpu_seconds: f(row, "user_seconds") + f(row, "sys_seconds"),
        executed,
        executed_unit: unit,
        schema_version: i(row, "schema_version"),
        executed_nodes: i(row, "executed_nodes"),
        executed_tests: i(row, "executed_tests"),
        passed_tests: i(row, "passed_tests"),
        commit: {
            let c = s(row, "commit");
            if c.is_empty() { "unknown".to_string() } else { c.to_string() }
        },
        producer: if producer.is_empty() { "validate.sh".into() } else { producer.into() },
    })
}

// ------------------------------------------------------------------ estimate

/// Minimum samples before a scope is reported (`MIN` in validate.sh:993).
const MIN_SAMPLES: usize = 3;

fn human(secs: f64) -> String {
    let x = (secs + 0.5) as i64;
    let (h, m, s) = (x / 3600, (x % 3600) / 60, x % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

fn emit(mut v: Vec<f64>, scope: &str) -> String {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    let lo = v[0];
    let hi = v[n - 1];
    let md = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
    if (lo - hi).abs() < f64::EPSILON {
        format!("~{} ({scope}, n={n})", human(md))
    } else {
        format!("~{} (median; range {}-{}; {scope}, n={n})", human(md), human(lo), human(hi))
    }
}

/// Port of `history_estimate` (validate.sh:936).
///
/// Only successful runs of the SAME profile count — a fast-failing or timed-out
/// run is not a representative completion time. Buckets degrade from
/// (cache, host) to (cache, any host) to (any cache, any host) and, when even
/// the broadest is too thin, SAY SO rather than fabricating a number.
pub fn history_estimate(
    rows: &[serde_json::Value],
    profile: &str,
    cache_state: &str,
    host: &str,
    have_ledger: bool,
) -> String {
    if !have_ledger {
        return "no measured estimate yet (no run-history ledger; this run seeds it)".into();
    }
    let (mut t1, mut t2, mut t3) = (Vec::new(), Vec::new(), Vec::new());
    for row in rows {
        if s(row, "profile") != profile || s(row, "result") != "pass" {
            continue;
        }
        let w = f(row, "real_seconds");
        if w <= 0.0 {
            continue;
        }
        t3.push(w);
        if s(row, "cache_state") == cache_state {
            t2.push(w);
            if s(row, "host") == host {
                t1.push(w);
            }
        }
    }
    if t1.len() >= MIN_SAMPLES {
        emit(t1, &format!("{cache_state} cache, {host}, this profile"))
    } else if t2.len() >= MIN_SAMPLES {
        emit(t2, &format!("{cache_state} cache, any host, this profile"))
    } else if t3.len() >= MIN_SAMPLES {
        emit(
            t3,
            &format!(
                "MIXED warm/cold -- no {cache_state}-specific history yet, treat as a wide prior; \
                 this profile"
            ),
        )
    } else {
        format!(
            "insufficient history to estimate (only {} prior successful {profile} run(s); need \
             >={MIN_SAMPLES}). Current cache: {cache_state}. This run seeds the estimate.",
            t3.len()
        )
    }
}

// ----------------------------------------------------------------- selective

/// Resolve the last-known-green baseline for `--selective`
/// (`resolve_selective_baseline`, validate.sh:4364).
///
/// Precedence: explicit `--baseline`, then `$HERMIT_LAST_GREEN_SHA`, then the
/// most recent passing ledger row (preferring this slot). Only a commit that
/// EXISTS locally is returned; anything else yields `None` so selection fails
/// safe to the full lane. Never fail-open on a stale or missing baseline.
pub fn selective_baseline(
    rows: &[serde_json::Value],
    explicit: Option<&str>,
    slot: &str,
    commit_exists: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let mut sha: Option<String> = explicit.map(|s| s.to_string());
    if sha.is_none() {
        sha = std::env::var("HERMIT_LAST_GREEN_SHA").ok().filter(|v| !v.is_empty());
    }
    if sha.is_none() {
        // `tail -n 1` in the bash: LAST matching line, i.e. append order, not
        // finished_at order. Preserved so both drivers pick the same baseline
        // from the same shard.
        let pick = |want_slot: Option<&str>| -> Option<String> {
            rows.iter()
                .rev()
                .find(|r| {
                    s(r, "result") == "pass"
                        && s(r, "commit") != "unknown"
                        && !s(r, "commit").is_empty()
                        && want_slot.map(|w| s(r, "slot") == w).unwrap_or(true)
                })
                .map(|r| s(r, "commit").to_string())
        };
        sha = pick(Some(slot)).or_else(|| pick(None));
    }
    let sha = sha?;
    if commit_exists(&sha) { Some(sha) } else { None }
}

// ----------------------------------------------------------------- self-test

/// Inert brackets. These construct synthetic ledger rows in memory; nothing here
/// reads the real ledger, runs a gate, or publishes anything.
pub fn self_test() -> Result<String, String> {
    let base = |extra: serde_json::Value| -> serde_json::Value {
        let mut v = serde_json::json!({
            "tree": "T", "profile": "full", "host": "h1", "toolchain": "rustc 1.0",
            "selection_mode": "full", "result": "pass", "commit_anchored": true,
            "tree_dirty": false, "failures": 0, "commit": "c0ffee",
            "finished_at": "2026-08-07T00:00:00Z", "real_seconds": 100,
        });
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in e {
                o.insert(k.clone(), val.clone());
            }
        }
        v
    };
    let key = CacheKey { tree: "T", profile: "full", host: "h1", toolchain: "rustc 1.0" };

    // POSITIVE, both producers. A predicate that refuses everything would look
    // correct with negatives alone, so each authority gets a counted accept.
    let rs_pass = base(serde_json::json!({
        "producer": "hermit-validate-rs", "executed_tests": 873, "executed_nodes": 47,
        "gates_expected": 47, "gates_run": 47,
        "coverage": {"planned_test_nodes": 20, "executed_test_nodes": 20, "absent_nodes": []},
    }));
    let sh_pass = base(serde_json::json!({
        "producer": "validate.sh", "executed_tests": 1234, "gates_expected": 12, "gates_run": 12,
    }));
    let mut accepted = 0usize;
    for (why, row) in [("validate.rs row", &rs_pass), ("validate.sh row", &sh_pass)] {
        if cache_lookup(std::slice::from_ref(row), "pass", &key).is_none() {
            return Err(format!("cache: a fully qualifying {why} must be a HIT"));
        }
        accepted += 1;
    }

    // NEGATIVE: every single missing condition must REFUSE. Each row below is
    // the positive row with exactly one field spoiled, so a refusal is
    // attributable to that field and nothing else.
    let negatives: Vec<(&str, serde_json::Value)> = vec![
        ("different tree", base(serde_json::json!({"tree": "OTHER", "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("different profile", base(serde_json::json!({"profile": "quick", "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("different host", base(serde_json::json!({"host": "h2", "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("different toolchain", base(serde_json::json!({"toolchain": "rustc 2.0", "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("selective run", base(serde_json::json!({"selection_mode": "selective", "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("not commit-anchored", base(serde_json::json!({"commit_anchored": false, "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("dirty tree", base(serde_json::json!({"tree_dirty": true, "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("nonzero failures", base(serde_json::json!({"failures": 1, "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("validate.rs row with no executed_tests", base(serde_json::json!({"producer": "validate.rs", "executed_nodes": 47, "gates_expected": 47, "gates_run": 47, "coverage": {"planned_test_nodes": 20, "executed_test_nodes": 20, "absent_nodes": []}}))),
        ("validate.rs row with zero executed_tests", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 0, "executed_nodes": 47, "gates_expected": 47, "gates_run": 47, "coverage": {"planned_test_nodes": 20, "executed_test_nodes": 20, "absent_nodes": []}}))),
        ("zero executed nodes", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 873, "executed_nodes": 0, "coverage": {"executed_test_nodes": 0, "absent_nodes": []}}))),
        ("absent coverage block", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 873, "executed_nodes": 5}))),
        ("planned node never ran", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 873, "executed_nodes": 5, "coverage": {"executed_test_nodes": 4, "absent_nodes": ["test.x"]}}))),
        ("gates_run below gates_expected", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 873, "executed_nodes": 5, "gates_expected": 47, "gates_run": 12, "coverage": {"executed_test_nodes": 5, "absent_nodes": []}}))),
        ("bash row with zero executed_tests", base(serde_json::json!({"producer": "validate.sh", "executed_tests": 0}))),
        ("bash row with no executed_tests", base(serde_json::json!({"producer": "validate.sh"}))),
        // The cross-producer trap this module exists to close: a validate.rs row
        // must NOT be admitted by the bash counter, and vice versa.
        ("validate.rs row carrying only executed_tests", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 999}))),
        ("bash row carrying only executed_nodes", base(serde_json::json!({"producer": "validate.sh", "executed_nodes": 999}))),
        ("unknown producer", base(serde_json::json!({"producer": "some-other-tool", "executed_nodes": 9, "executed_tests": 9}))),
    ];
    let mut refused = 0usize;
    for (why, row) in &negatives {
        if cache_lookup(std::slice::from_ref(row), "pass", &key).is_some() {
            return Err(format!("cache: a row with {why} must NOT be a hit"));
        }
        refused += 1;
    }

    // The reused count must never be relabelled: a node count must print as
    // node(s) and a test count as test(s).
    let hit = cache_lookup(std::slice::from_ref(&rs_pass), "pass", &key).unwrap();
    if hit.executed_unit != "node(s)" || hit.executed != 47 {
        return Err("cache: a validate.rs hit must report 47 node(s), not tests".into());
    }
    let hit = cache_lookup(std::slice::from_ref(&sh_pass), "pass", &key).unwrap();
    if hit.executed_unit != "test(s)" || hit.executed != 1234 {
        return Err("cache: a validate.sh hit must report 1234 test(s), not nodes".into());
    }

    // A FAIL lookup must not require the pass conditions (a fail is noted, not reused).
    let failing = base(serde_json::json!({
        "schema_version": crate::validate_cell_results::CELL_RESULTS_LEDGER_SCHEMA_VERSION,
        "result": "fail", "failures": 3, "producer": "hermit-validate-rs",
        "dag_jobs": 4, "concurrent_validates": 0,
        "gates_expected": 1, "gates_run": 1,
        "gates": [{"result": "fail", "exit_code": 1, "real_seconds": 5.0,
                   "failure_origin": "outer_gate"}],
        "cell_results": {"cells": [{"cell_verdict": {"state": "compared-and-diverged"}}]}
    }));
    if cache_lookup(std::slice::from_ref(&failing), "fail", &key).is_none() {
        return Err("cache: a prior FAIL record must be findable so it can be reported".into());
    }

    // The outer version is authoritative. A future shape that happens to keep
    // today's nested state spelling remains readable but cannot poison a pass
    // cache until this reader explicitly supports that schema.
    let mut newer_failing = failing.clone();
    newer_failing["schema_version"] = serde_json::json!(
        crate::validate_cell_results::CELL_RESULTS_LEDGER_SCHEMA_VERSION + 1
    );
    if cache_lookup(&[newer_failing, rs_pass.clone()], "pass", &key).is_none() {
        return Err(
            "cache: an unsupported newer cell-results schema must not gain failure authority"
                .into(),
        );
    }

    for role in ["reference", "cross"] {
        let mut parity_failure = failing.clone();
        parity_failure["schema_version"] = serde_json::json!(10);
        let state = if role == "cross" { "diverged" } else { "compared-and-diverged" };
        parity_failure["cell_results"] = serde_json::json!({"cells":[{
            "cell_verdict":{"state":"compared-and-matched"},
            "backend_parity":{"attempts":[{role:{"state":state}}]}
        }]});
        if cache_lookup(&[parity_failure.clone(), rs_pass.clone()], "pass", &key).is_some()
            || cache_lookup(&[rs_pass.clone(), parity_failure], "pass", &key).is_some()
        {
            return Err(format!("cache: an ordinary pass erased a schema-10 {role} divergence"));
        }
    }

    // The two historical orderings are both refused: fail-then-pass and
    // pass-then-fail. A cache key is content identity, so append order must not
    // decide whether a known-failing tree gets a zero-gate green.
    for rows in [
        vec![failing.clone(), rs_pass.clone()],
        vec![rs_pass.clone(), failing.clone()],
    ] {
        if cache_lookup(&rows, "pass", &key).is_some() {
            return Err("cache: a genuine same-key failure must latch over a PASS".into());
        }
    }

    // Environment and incomplete evidence remain non-poisoning.
    let environment = base(serde_json::json!({
        "result": "fail", "failures": 1, "producer": "validate.rs",
        "dag_jobs": 4, "concurrent_validates": 0, "known_flaky_failure": false,
        "gates_expected": 1, "gates_run": 1,
        "gates": [{"result": "fail", "exit_code": 127, "real_seconds": 0.1,
                   "failure_origin": "outer_gate"}]
    }));
    if cache_lookup(&[environment, rs_pass.clone()], "pass", &key).is_none() {
        return Err("cache: an environment fault must not poison a qualifying PASS".into());
    }
    let contended = base(serde_json::json!({
        "result": "fail", "failures": 1, "producer": "validate.rs",
        "dag_jobs": 16, "concurrent_validates": 2, "known_flaky_failure": false,
        "gates_expected": 1, "gates_run": 1,
        "gates": [{"result": "fail", "exit_code": 1, "real_seconds": 5.0,
                   "failure_origin": "outer_gate"}]
    }));
    if cache_lookup(&[contended, rs_pass], "pass", &key).is_none() {
        return Err("cache: an unconfirmed contended red must not poison a qualifying PASS".into());
    }

    let missing_origin = base(serde_json::json!({
        "result": "fail", "failures": 1, "producer": "validate.rs",
        "dag_jobs": 4, "concurrent_validates": 0, "known_flaky_failure": false,
        "gates_expected": 1, "gates_run": 1,
        "gates": [{"result": "fail", "exit_code": 1, "real_seconds": 5.0}]
    }));
    let incomplete = base(serde_json::json!({
        "result": "fail", "failures": 1, "producer": "validate.rs",
        "dag_jobs": 4, "concurrent_validates": 0, "known_flaky_failure": false,
        "gates_expected": 2, "gates_run": 1,
        "gates": [{"result": "fail", "exit_code": 1, "real_seconds": 5.0,
                   "failure_origin": "outer_gate"}]
    }));
    let subsecond = base(serde_json::json!({
        "result": "fail", "failures": 1, "producer": "validate.rs",
        "real_seconds": 0.5,
        "dag_jobs": 4, "concurrent_validates": 0, "known_flaky_failure": false,
        "gates_expected": 1, "gates_run": 1,
        "gates": [{"result": "fail", "exit_code": 1, "real_seconds": 0.1,
                   "failure_origin": "outer_gate"}]
    }));
    for (why, row) in [
        ("missing failure origin", missing_origin),
        ("incomplete gate accounting", incomplete),
        ("sub-second all-red collapse", subsecond),
    ] {
        if cache_lookup(&[row, sh_pass.clone()], "pass", &key).is_none() {
            return Err(format!("cache: {why} must not poison a qualifying PASS"));
        }
    }
    let named_gate_without_aggregate = base(serde_json::json!({
        "result": "fail", "failures": 0, "producer": "validate.rs",
        "dag_jobs": 4, "concurrent_validates": 0, "known_flaky_failure": false,
        "gates_expected": 1, "gates_run": 1,
        "gates": [{"result": "fail", "exit_code": 1, "real_seconds": 5.0,
                   "failure_origin": "outer_gate"}]
    }));
    if cache_lookup(&[named_gate_without_aggregate, sh_pass.clone()], "pass", &key).is_some() {
        return Err(
            "cache: a bound named-gate failure must latch without an aggregate count".into(),
        );
    }

    let flaky_unconfirmed = base(serde_json::json!({
        "result": "fail", "failures": 1, "producer": "hermit-validate-rs",
        "dag_jobs": 4, "concurrent_validates": 0, "known_flaky_failure": true,
        "gates_expected": 1, "gates_run": 1,
        "gates": [{"result": "fail", "exit_code": 1, "real_seconds": 5.0,
                   "failure_origin": "outer_gate"}]
    }));
    if cache_lookup(&[flaky_unconfirmed.clone(), sh_pass.clone()], "pass", &key).is_none() {
        return Err("cache: unconfirmed known-flaky failure must remain NeedsRerun".into());
    }
    let mut flaky_confirmed = flaky_unconfirmed;
    flaky_confirmed["solo_rerun_confirmation"] = serde_json::Value::Bool(true);
    if cache_lookup(&[flaky_confirmed, sh_pass], "pass", &key).is_some() {
        return Err("cache: solo-confirmed known-flaky failure must latch".into());
    }

    // Estimate brackets: below MIN it must SAY it is insufficient; at/above MIN
    // it must produce a median. A silently-fabricated number is the failure mode.
    let sample = |secs: i64| base(serde_json::json!({"cache_state": "warm", "real_seconds": secs, "producer": "validate.rs"}));
    let thin: Vec<serde_json::Value> = (0..MIN_SAMPLES - 1).map(|i| sample(100 + i as i64)).collect();
    let est = history_estimate(&thin, "full", "warm", "h1", true);
    if !est.contains("insufficient history") {
        return Err(format!("estimate: {} samples must be reported as insufficient", thin.len()));
    }
    let enough: Vec<serde_json::Value> = vec![sample(60), sample(120), sample(180)];
    let est = history_estimate(&enough, "full", "warm", "h1", true);
    if !est.starts_with("~2m00s") || !est.contains("n=3") {
        return Err(format!("estimate: median of 60/120/180 must be ~2m00s, got {est}"));
    }
    if !history_estimate(&enough, "full", "warm", "h1", false).contains("no run-history ledger") {
        return Err("estimate: a missing ledger must say so".into());
    }
    // A failing run must never contribute to a completion-time estimate.
    let poisoned: Vec<serde_json::Value> = vec![
        sample(60),
        sample(120),
        base(serde_json::json!({"cache_state": "warm", "real_seconds": 5, "result": "fail", "producer": "validate.rs"})),
    ];
    if !history_estimate(&poisoned, "full", "warm", "h1", true).contains("insufficient history") {
        return Err("estimate: a failing run must not count as a completion sample".into());
    }

    // Selective-baseline brackets: a nonexistent commit must be REFUSED (so the
    // caller falls back to the full lane) and an existing one ACCEPTED.
    let ledger_rows = vec![
        base(serde_json::json!({"slot": "other", "commit": "aaa", "producer": "validate.rs"})),
        base(serde_json::json!({"slot": "mine", "commit": "bbb", "producer": "validate.rs"})),
    ];
    let exists_all = |_: &str| true;
    let exists_none = |_: &str| false;
    let seen: BTreeSet<String> = ledger_rows.iter().map(|r| s(r, "commit").to_string()).collect();
    if seen.len() != 2 {
        return Err("selective: fixture rows must carry distinct commits".into());
    }
    if selective_baseline(&ledger_rows, None, "mine", &exists_all).as_deref() != Some("bbb") {
        return Err("selective: this slot's newest passing commit must win".into());
    }
    if selective_baseline(&ledger_rows, Some("cafe"), "mine", &exists_all).as_deref() != Some("cafe") {
        return Err("selective: an explicit --baseline must win".into());
    }
    if selective_baseline(&ledger_rows, Some("cafe"), "mine", &exists_none).is_some() {
        return Err("selective: a baseline absent from this checkout must be REFUSED".into());
    }
    refused += 1;
    accepted += 2;
    Ok(format!(
        "history: cache bracketed {accepted} accept / {refused} refuse (incl. both \
         cross-producer counter traps), estimate bracketed thin/median/no-ledger/fail-poison, \
         selective baseline bracketed slot-preference/explicit/missing-commit"
    ))
}
