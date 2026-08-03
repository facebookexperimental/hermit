#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Power-to-weight ranking for Hermit CI nodes.
//!
//! Each portable-DAG node has a declared scheduling weight and a measured
//! selection frequency. This tool joins the two so rarely selected, relatively
//! heavy nodes can be reviewed for moving off the per-commit critical path.
//!
//!   * WEIGHT  = `hint.est_duration_s` from `ci/dag/portable.json`. Despite the
//!               legacy field name, these values are UNMEASURED scheduling
//!               hints (see ci/dag/README). This tool treats them as unitless,
//!               ordinal weights and never presents them as elapsed seconds.
//!   * POWER   = selection frequency: over a sample of recent commits, the
//!               fraction that `ci/select-tests.rs` would actually run this node
//!               for. A node almost never triggered by real changes delivers
//!               little value per CI *run*; a node triggered by nearly every
//!               change is load-bearing.
//!
//! POWER-TO-WEIGHT = selection_rate / normalized_weight. High = cheap and
//! frequently needed (keep hot). Low = expensive and rarely needed (nightly
//! candidate).
//!
//! IMPORTANT HONESTY CAVEATS (see the presenting-quantitative-data skill):
//!   * selection_rate is measured against PAST commits; it predicts future
//!     value only if the change mix stays similar. It is a proxy, not coverage.
//!   * "rarely selected" ≠ "safe to delete". A rarely-triggered test may guard a
//!     rarely-touched but critical subsystem. This tool RANKS and FLAGS; it does
//!     not decide. Downranking = move to nightly, never silently drop.
//!   * WEIGHT is not measured; the ranking is only as good as those declared
//!     ordinal hints.
//!
//! Usage:
//!   ci/power-to-weight.rs                       # sample last 100 commits, human table
//!   ci/power-to-weight.rs --sample 300          # wider history window
//!   ci/power-to-weight.rs --format csv > pw.csv # machine-readable artifact
//!   ci/power-to-weight.rs --rev origin/main     # sample history from a ref
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
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn fail(msg: &str) -> ! {
    eprintln!("power-to-weight: {msg}");
    std::process::exit(2);
}

struct Node {
    tag: String,
    declared_weight: u64,
    classification: String,
    times_selected: u64,
}

fn repo_root() -> PathBuf {
    if let Ok(out) = Command::new("git").args(["rev-parse", "--show-toplevel"]).output() {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn load_nodes(dag_path: &Path) -> BTreeMap<String, Node> {
    let raw = std::fs::read_to_string(dag_path)
        .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", dag_path.display())));
    let v: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| fail(&format!("invalid JSON in {}: {e}", dag_path.display())));
    let mut nodes = BTreeMap::new();
    for s in v["steps"].as_array().unwrap_or(&vec![]) {
        let tag = format!(
            "{}.{}",
            s["group"].as_str().unwrap_or(""),
            s["job"].as_str().unwrap_or("")
        );
        let hint = &s["hint"];
        nodes.insert(
            tag.clone(),
            Node {
                tag,
                declared_weight: hint["est_duration_s"].as_u64().unwrap_or(0),
                classification: hint["classification"].as_str().unwrap_or("?").to_string(),
                times_selected: 0,
            },
        );
    }
    nodes
}

/// The commit SHAs to sample, newest first.
fn sample_commits(rev: &str, n: usize) -> Vec<String> {
    let out = Command::new("git")
        .args(["log", "--format=%H", "-n", &n.to_string(), rev])
        .output()
        .unwrap_or_else(|e| fail(&format!("git log failed: {e}")));
    if !out.status.success() {
        fail(&format!("git log {rev} failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Changed files in one commit (vs its first parent).
fn commit_files(sha: &str) -> Vec<String> {
    let out = Command::new("git")
        .args(["show", "--pretty=", "--name-only", sha])
        .output()
        .unwrap_or_else(|e| fail(&format!("git show failed: {e}")));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Ask select-tests.rs which nodes a file set selects. Returns (decision, nodes).
/// Reusing the selector keeps selection logic in ONE place.
fn select_nodes(selector: &Path, files: &[String]) -> (String, Vec<String>) {
    use std::io::Write;
    let mut child = Command::new(selector)
        .args(["--files", "-", "--format", "json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| fail(&format!("cannot run {}: {e}", selector.display())));
    {
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(files.join("\n").as_bytes()).ok();
    }
    let out = child.wait_with_output().unwrap_or_else(|e| fail(&format!("selector wait: {e}")));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        fail(&format!("selector emitted non-JSON: {e}: {}", String::from_utf8_lossy(&out.stdout)))
    });
    let decision = v["decision"].as_str().unwrap_or("full").to_string();
    let nodes = v["nodes"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    (decision, nodes)
}

fn main() {
    rust_script_prelude::init();
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!(
            "\
Usage: ci/power-to-weight.rs [--sample N] [--rev <ref>] [--format human|csv]

Rank portable-DAG nodes by power-to-weight: measured selection frequency over
recent history (POWER) against an unmeasured, unitless scheduling hint (WEIGHT).
Flags relatively heavy, rarely-selected nodes as review candidates.

  --sample N   Number of recent commits to sample (default 100).
  --rev <ref>  History to sample from (default HEAD).
  --format     human (default) or csv.

WEIGHT is explicitly NOT MEASURED (ci/dag/README); POWER states the sampled
commit count and revision. Low power-to-weight means 'review for moving to
nightly', never 'safe to delete'.
"
        );
        return;
    }

    let mut sample = 100usize;
    let mut rev = "HEAD".to_string();
    let mut format = "human".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--sample" => {
                sample = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or_else(|| fail("--sample needs a number"));
                i += 2;
            }
            "--rev" => {
                rev = args.get(i + 1).cloned().unwrap_or_else(|| fail("--rev needs a ref"));
                i += 2;
            }
            "--format" => {
                format = args.get(i + 1).cloned().unwrap_or_else(|| fail("--format needs a value"));
                i += 2;
            }
            other => fail(&format!("unknown argument: {other} (see --help)")),
        }
    }

    let root = repo_root();
    let selector = root.join("ci/select-tests.rs");
    if !selector.exists() {
        fail(&format!("selector not found at {}", selector.display()));
    }
    let mut nodes = load_nodes(&root.join("ci/dag/portable.json"));

    let commits = sample_commits(&rev, sample);
    if commits.is_empty() {
        fail("no commits sampled");
    }
    let sample_newest = commits.first().expect("non-empty commit sample");
    let sample_oldest = commits.last().expect("non-empty commit sample");
    let sample_newest_short: String = sample_newest.chars().take(12).collect();
    let sample_oldest_short: String = sample_oldest.chars().take(12).collect();
    let mut n_full = 0u64;
    let mut n_skip = 0u64;
    let mut n_selective = 0u64;

    for sha in &commits {
        let files = commit_files(sha);
        let (decision, selected) = select_nodes(&selector, &files);
        match decision.as_str() {
            "full" => n_full += 1,
            "skip" => n_skip += 1,
            _ => n_selective += 1,
        }
        for nd in selected {
            if let Some(node) = nodes.get_mut(&nd) {
                node.times_selected += 1;
            }
        }
    }

    let total = commits.len() as f64;
    // Normalize the declared ordinal weight so power-to-weight is unit-free.
    let max_weight = nodes.values().map(|n| n.declared_weight).max().unwrap_or(1).max(1) as f64;

    // Sort ascending by power-to-weight (worst first = best pruning candidates).
    let mut ranked: Vec<&Node> = nodes.values().collect();
    let p2w = |n: &Node| {
        let rate = n.times_selected as f64 / total;
        let w = (n.declared_weight as f64 / max_weight).max(1e-9);
        rate / w
    };
    ranked.sort_by(|a, b| {
        p2w(a).partial_cmp(&p2w(b)).unwrap().then(b.declared_weight.cmp(&a.declared_weight))
    });

    // This is a configured review heuristic over an unmeasured ordinal weight
    // and a measured selection rate, not a claim about elapsed seconds.
    let is_nightly_candidate = |n: &Node| {
        let rate = n.times_selected as f64 / total;
        n.declared_weight >= 120 && rate < 0.34
    };

    match format.as_str() {
        "csv" => {
            println!("node,declared_unmeasured_weight,classification,times_selected,sample_size,sample_newest_sha,sample_oldest_sha,selection_rate,power_to_weight,review_candidate");
            for n in &ranked {
                let rate = n.times_selected as f64 / total;
                println!(
                    "{},{},{},{},{},{},{},{:.3},{:.3},{}",
                    n.tag,
                    n.declared_weight,
                    n.classification,
                    n.times_selected,
                    commits.len(),
                    sample_newest,
                    sample_oldest,
                    rate,
                    p2w(n),
                    is_nightly_candidate(n),
                );
            }
        }
        "human" => {
            println!("Power-to-weight ranking over {} commit(s) from {rev}", commits.len());
            println!(
                "  decisions: {n_selective} selective, {n_skip} skip, {n_full} full \
                 ({:.0}% of commits ran the full suite)",
                100.0 * n_full as f64 / total
            );
            println!(
                "  WEIGHT = declared UNMEASURED, unitless scheduling hint; \
                 POWER = measured selection rate (n={}, requested revision={rev}, \
                 commit window={}..{}).\n",
                commits.len(),
                sample_oldest_short,
                sample_newest_short
            );
            println!(
                "{:<38} {:>6} {:>14} {:>9} {:>7}  {}",
                "node", "weight", "classification", "sel_rate", "p2w", "flag"
            );
            for n in &ranked {
                let rate = n.times_selected as f64 / total;
                let flag = if is_nightly_candidate(n) { "NIGHTLY-CANDIDATE" } else { "" };
                println!(
                    "{:<38} {:>6} {:>14} {:>8.0}% {:>7.3}  {}",
                    n.tag,
                    n.declared_weight,
                    n.classification,
                    100.0 * rate,
                    p2w(n),
                    flag
                );
            }
            let candidates: Vec<&&Node> = ranked.iter().filter(|n| is_nightly_candidate(n)).collect();
            println!(
                "\n{} node(s) flagged NIGHTLY-CANDIDATE (declared unmeasured weight >= 120 \
                 AND measured selection < 34%; configured review heuristic, n={}).",
                candidates.len(),
                commits.len()
            );
            println!(
                "These are ranked, not condemned: moving one to nightly trades per-commit latency for\n\
                 slower regression detection on rarely-touched subsystems. Confirm with the owning area\n\
                 and replace ordinal weights with measured durations before making a cost claim."
            );
        }
        other => fail(&format!("unknown --format {other} (human|csv)")),
    }
}
