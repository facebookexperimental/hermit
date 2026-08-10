// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Working-envelope measurement (`--envelope-only` / `--envelope-compare FILE`).
//!
//! # This is a MEASUREMENT, not a gate
//!
//! `run_envelope` (validate.sh:4173) is explicit about it: "known failures (e.g.
//! an unsupported syscall on this host) lower a count but never abort
//! validation." So the envelope DAG runs with keep-going forced on, and a probe
//! failure only decrements a count. The only ways this profile exits nonzero are
//! a failed workspace build (validate.sh:4878-4881) and a `--envelope-compare`
//! regression (validate.sh:4242).
//!
//! # Provenance of the probe table
//!
//! `ENVELOPE_PROBES` is three rows, dumped mechanically from the real bash with
//! `declare -p ENVELOPE_PROBES` in the same instrumented copy that produced
//! `ci/super/gates.json`:
//!
//! ```text
//! declare -ar ENVELOPE_PROBES=([0]="true|/bin/true" [1]="echo|/bin/echo hermit-envelope"
//!                              [2]="date|/bin/date -u +%Y")
//! declare -ar HERMIT_RUN_ARGS=([0]="run" [1]="--base-env=minimal"
//!                              [2]="--no-virtualize-cpuid" [3]="--max-timeslice=disabled")
//! L4_REPS=20   HERMIT_SMOKE_TIMEOUT=30s
//! ```
//!
//! The bash split each `"label|cmd"` on whitespace with `read -r -a`, so the
//! argv below is that split, not a re-reading of the intent.
//!
//! # Why one node per assurance level
//!
//! Each level is separately boxed and separately timed, and the L4 stress node
//! DEPENDS on the L2 node — which is exactly the bash's `if ((p2 == 1))` guard,
//! expressed structurally: when L2 fails, L4 is *skipped* and therefore scores
//! 0, without the driver needing a conditional that could drift from the guard.

use std::path::Path;

use safe_ci_dag_runner::model::Step;
use safe_ci_dag_runner::model::StepOutcome;

use crate::validate_plan::node;
use crate::validate_plan::shell_join;

/// One end-to-end scenario measured at every assurance level.
pub struct EnvelopeProbe {
    pub label: &'static str,
    pub argv: &'static [&'static str],
}

/// `ENVELOPE_PROBES` (validate.sh:1251), split exactly as `read -r -a` split it.
pub const PROBES: &[EnvelopeProbe] = &[
    EnvelopeProbe { label: "true", argv: &["/bin/true"] },
    EnvelopeProbe { label: "echo", argv: &["/bin/echo", "hermit-envelope"] },
    EnvelopeProbe { label: "date", argv: &["/bin/date", "-u", "+%Y"] },
];

/// `HERMIT_RUN_ARGS` (validate.sh:1230).
pub const HERMIT_RUN_ARGS: &[&str] =
    &["run", "--base-env=minimal", "--no-virtualize-cpuid", "--max-timeslice=disabled"];

/// `L4_REPS` (validate.sh:1256).
pub const L4_REPS_DEFAULT: i64 = 20;

/// `HERMIT_SMOKE_TIMEOUT` (validate.sh:1086), as seconds.
const SMOKE_TIMEOUT_S: i64 = 30;

const PROBE_MEM_BYTES: i64 = 4 * 1024 * 1024 * 1024;

/// The five assurance levels a probe is measured at, in report order.
pub const LEVELS: &[&str] = &["l1", "l2", "l3", "l4", "rr"];

/// `L4_REPS`, honoring the environment override the bash exposed.
pub fn l4_reps() -> i64 {
    std::env::var("L4_REPS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(L4_REPS_DEFAULT)
}

/// `$ENVELOPE_JSON` (validate.sh:1257), default `$ROOT_DIR/envelope.json`.
pub fn json_path(root: &Path) -> std::path::PathBuf {
    match std::env::var("ENVELOPE_JSON") {
        Ok(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => root.join("envelope.json"),
    }
}

/// The `hermit` invocation for one level, reproducing `_envelope_level`
/// (validate.sh:4161). stdin is closed so no probe can block on input.
fn level_command(hermit_bin: &str, flags: &[&str], argv: &[&str]) -> String {
    let mut v: Vec<String> = vec![hermit_bin.to_string()];
    v.extend(HERMIT_RUN_ARGS.iter().map(|s| s.to_string()));
    v.extend(flags.iter().map(|s| s.to_string()));
    v.push("--".to_string());
    v.extend(argv.iter().map(|s| s.to_string()));
    format!("{} </dev/null", shell_join(&v))
}

/// Build every envelope node. `build_dep` is the workspace build the bash gates
/// the whole measurement behind (validate.sh:4878).
pub fn nodes(hermit_bin: &str, reps: i64, build_dep: &str) -> Vec<Step> {
    let mut out = Vec::new();
    for p in PROBES {
        let job = |lvl: &str| format!("{}_{lvl}", p.label);
        out.push(node(
            "envelope",
            &job("l1"),
            &format!("envelope {}: L1 hermit run --strict", p.label),
            level_command(hermit_bin, &["--strict"], p.argv),
            vec![build_dep.to_string()],
            SMOKE_TIMEOUT_S,
            SMOKE_TIMEOUT_S * 2,
            PROBE_MEM_BYTES,
        ));
        out.push(node(
            "envelope",
            &job("l2"),
            &format!("envelope {}: L2 --strict --verify", p.label),
            level_command(hermit_bin, &["--strict", "--verify"], p.argv),
            vec![build_dep.to_string()],
            SMOKE_TIMEOUT_S,
            SMOKE_TIMEOUT_S * 2,
            PROBE_MEM_BYTES,
        ));
        out.push(node(
            "envelope",
            &job("l3"),
            &format!("envelope {}: L3 --verify --detlog-heap --detlog-stack", p.label),
            level_command(
                hermit_bin,
                &["--strict", "--verify", "--detlog-heap", "--detlog-stack"],
                p.argv,
            ),
            vec![build_dep.to_string()],
            SMOKE_TIMEOUT_S,
            SMOKE_TIMEOUT_S * 2,
            PROBE_MEM_BYTES,
        ));
        // L4 depends on L2: the bash only counted L4 when L2 passed, and a
        // dependency edge reproduces that without a second conditional. Each
        // repetition keeps its own 30s bound, and the loop stops at the first
        // divergence exactly as `|| { ok=0; break; }` did.
        let l2_cmd = level_command(hermit_bin, &["--strict", "--verify"], p.argv);
        out.push(node(
            "envelope",
            &job("l4"),
            &format!("envelope {}: L4 = L2 stress x{reps} (no divergence)", p.label),
            format!(
                "i=0; while [ $i -lt {reps} ]; do timeout {SMOKE_TIMEOUT_S}s {l2_cmd} || exit 1; \
                 i=$((i+1)); done"
            ),
            vec![format!("envelope.{}", job("l2"))],
            SMOKE_TIMEOUT_S * reps + 60,
            SMOKE_TIMEOUT_S * reps * 2,
            PROBE_MEM_BYTES,
        ));
        // `record start --verify` records, replays non-interactively, diffs the
        // two logs, and deletes the recording on success -- a self-contained rr
        // probe with a clean exit status (validate.sh:4205-4211).
        let rr_timeout = std::env::var("HERMIT_RR_TIMEOUT")
            .ok()
            .and_then(|v| v.trim_end_matches('s').parse::<i64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(SMOKE_TIMEOUT_S);
        let mut rr: Vec<String> = vec![hermit_bin.to_string(), "record".into(), "start".into(), "--verify".into(), "--".into()];
        rr.extend(p.argv.iter().map(|s| s.to_string()));
        out.push(node(
            "envelope",
            &job("rr"),
            &format!("envelope {}: rr record/replay end-to-end", p.label),
            format!("{} </dev/null", shell_join(&rr)),
            vec![build_dep.to_string()],
            rr_timeout,
            rr_timeout * 2,
            PROBE_MEM_BYTES,
        ));
    }
    out
}

/// The workspace build node the measurement hangs off.
pub fn build_node(gate_dep: &str) -> Step {
    node(
        "envelope",
        "build",
        "Build workspace for envelope measurement",
        "cargo build --workspace --features third-party-backends".to_string(),
        vec![gate_dep.to_string()],
        3600,
        7200,
        16 * 1024 * 1024 * 1024,
    )
}

/// Per-probe, per-level pass bits derived from typed outcomes.
///
/// A node that never ran (skipped because its dependency failed) scores 0, which
/// is exactly what the bash's `p4=0` default did when L2 failed.
pub fn score(outcomes: &[StepOutcome], reps: i64, commit: &str) -> serde_json::Value {
    let passed = |tag: &str| -> i64 {
        outcomes.iter().find(|o| o.tag == tag).map(|o| i64::from(o.ok && !o.aborted)).unwrap_or(0)
    };
    let mut totals = [0i64; 5];
    let mut probes = Vec::new();
    for p in PROBES {
        let mut row = serde_json::Map::new();
        row.insert("probe".into(), serde_json::Value::String(p.label.to_string()));
        for (i, lvl) in LEVELS.iter().enumerate() {
            let bit = passed(&format!("envelope.{}_{lvl}", p.label));
            totals[i] += bit;
            row.insert((*lvl).to_string(), serde_json::Value::from(bit));
        }
        probes.push(serde_json::Value::Object(row));
    }
    serde_json::json!({
        "l1_pass": totals[0],
        "l2_pass": totals[1],
        "l3_pass": totals[2],
        "l4_pass": totals[3],
        "rr_pass": totals[4],
        "total": PROBES.len(),
        "commit": commit,
        "l4_reps": reps,
        "probes": probes,
    })
}

/// Serialize the vector in `validate.sh`'s EXACT key order.
///
/// `serde_json`'s default map is a `BTreeMap`, so a plain `to_string` sorts the
/// keys alphabetically and emits `{"commit":...` where the bash emitted
/// `{"l1_pass":...`. Consumers parse rather than pattern-match, so the ordering
/// is not load-bearing today — but the bash's `printf` order IS the documented
/// shape, and quietly reordering a published artifact is the kind of drift this
/// port exists to avoid.
///
/// (Separately: `scripts/progress-report.sh:103` greps the LOG for
/// `^\{"l1_pass"`, which has never matched because `run_envelope` prints the JSON
/// with a two-space indent. That consumer already falls back to reading
/// `$ENVELOPE_JSON`, which is why the layout below is preserved as-is rather than
/// "fixed" here.)
pub fn to_ordered_json(v: &serde_json::Value) -> String {
    let n = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let probes: Vec<String> = v
        .get("probes")
        .and_then(|p| p.as_array())
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    let g = |k: &str| r.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
                    format!(
                        r#"{{"probe":"{}","l1":{},"l2":{},"l3":{},"l4":{},"rr":{}}}"#,
                        r.get("probe").and_then(|x| x.as_str()).unwrap_or(""),
                        g("l1"),
                        g("l2"),
                        g("l3"),
                        g("l4"),
                        g("rr")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    format!(
        r#"{{"l1_pass":{},"l2_pass":{},"l3_pass":{},"l4_pass":{},"rr_pass":{},"total":{},"commit":"{}","l4_reps":{},"probes":[{}]}}"#,
        n("l1_pass"),
        n("l2_pass"),
        n("l3_pass"),
        n("l4_pass"),
        n("rr_pass"),
        n("total"),
        v.get("commit").and_then(|c| c.as_str()).unwrap_or("unknown"),
        n("l4_reps"),
        probes.join(",")
    )
}

/// Reproduce `run_envelope`'s human summary, byte-for-byte in layout.
///
/// The two-space indent on the JSON line is `validate.sh`'s
/// (`printf "  %s\n" "$ENVELOPE_LAST_JSON"`), preserved deliberately even though
/// it means `scripts/progress-report.sh`'s `grep -E '^\{"l1_pass"'` has never
/// matched it — that consumer already falls back to reading `$ENVELOPE_JSON`,
/// and silently changing the layout would be a drift the port cannot justify.
pub fn print_summary(v: &serde_json::Value, reps: i64, json_file: &Path) {
    let g = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let total = g("total");
    let commit = v.get("commit").and_then(|c| c.as_str()).unwrap_or("unknown");
    println!("\n== Working-envelope vector (commit {commit}) ==");
    println!("  L1  hermit run --strict                          : {}/{total}", g("l1_pass"));
    println!("  L2  --strict --verify (bitwise identical)        : {}/{total}", g("l2_pass"));
    println!("  L3  --verify --detlog-heap --detlog-stack        : {}/{total}", g("l3_pass"));
    println!("  L4  L2 stress x{reps:<3} (no divergence)               : {}/{total}", g("l4_pass"));
    println!("  rr  record/replay end-to-end                     : {}/{total}", g("rr_pass"));
    println!("  total e2e probes                                 : {total}");
    println!("  JSON: {}", json_file.display());
    println!("  {}", to_ordered_json(v));
}

/// Compare against a baseline; any count that DECREASED is a regression.
///
/// Port of `envelope_compare` (validate.sh:4242) minus its `jq` dependency: the
/// bash returned 2 when `jq` was missing, which meant a host without `jq` could
/// not enforce monotonicity at all. Parsing with `serde_json` removes that
/// failure mode; an unreadable or malformed baseline still returns 2.
pub fn compare(current: &serde_json::Value, baseline: &Path) -> Result<bool, (u8, String)> {
    let text = std::fs::read_to_string(baseline)
        .map_err(|e| (2u8, format!("envelope-compare: cannot read baseline {}: {e}", baseline.display())))?;
    let base: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        (2u8, format!("envelope-compare: baseline {} is not valid JSON: {e}", baseline.display()))
    })?;
    let mut regressed = false;
    println!("\n== Envelope monotonicity vs {} ==", baseline.display());
    for key in ["l1_pass", "l2_pass", "l3_pass", "l4_pass", "rr_pass", "total"] {
        let b = base.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        let c = current.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        if c < b {
            println!("  ❌ REGRESSION {key:<8} {c} < baseline {b}");
            regressed = true;
        } else {
            println!("  ✅ {key:<8} {c} >= baseline {b}");
        }
    }
    Ok(regressed)
}

/// Inert brackets: nothing here runs hermit or publishes anything.
pub fn self_test() -> Result<String, String> {
    // The node set must be exactly 5 per probe, and every one must be capped.
    let steps = nodes("/nonexistent/hermit", 3, "gate.manifest");
    let want = PROBES.len() * LEVELS.len();
    if steps.len() != want {
        return Err(format!("envelope built {} nodes, expected {want}", steps.len()));
    }
    for s in &steps {
        if s.timeout <= 0 || s.cpu_timeout <= 0 || s.hint.hard_mem_max_bytes.is_none() {
            return Err(format!("envelope node {} is not fully capped", s.tag()));
        }
    }
    // The L4 node must DEPEND on its own L2 node; that edge IS the `p2 == 1`
    // guard, so losing it would silently start counting L4 after an L2 failure.
    for p in PROBES {
        let l4 = steps
            .iter()
            .find(|s| s.job == format!("{}_l4", p.label))
            .ok_or_else(|| format!("envelope: no l4 node for {}", p.label))?;
        if l4.deps != vec![format!("envelope.{}_l2", p.label)] {
            return Err(format!(
                "envelope {} l4 must depend on its l2 node, found {:?}",
                p.label, l4.deps
            ));
        }
    }
    // Scoring bracket. Positive: a full sweep scores 3/3 everywhere. Negative: a
    // skipped (never-run) node scores 0, and an aborted one does too.
    let mk = |tag: String, ok: bool, aborted: bool| StepOutcome {
        tag,
        ok,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests: None,
        filtered_tests: None,
        returncode: Some(0),
        reason: String::new(),
        aborted,
    };
    let mut all: Vec<StepOutcome> = Vec::new();
    for p in PROBES {
        for lvl in LEVELS {
            all.push(mk(format!("envelope.{}_{lvl}", p.label), true, false));
        }
    }
    let full = score(&all, 3, "deadbee");
    for k in ["l1_pass", "l2_pass", "l3_pass", "l4_pass", "rr_pass"] {
        if full[k].as_i64() != Some(PROBES.len() as i64) {
            return Err(format!("envelope score: full sweep must give {k} = {}", PROBES.len()));
        }
    }
    // Drop every l4 outcome (as a dependency-skip would) and confirm l4_pass falls to 0.
    let no_l4: Vec<StepOutcome> =
        all.iter().filter(|o| !o.tag.ends_with("_l4")).cloned().collect();
    if score(&no_l4, 3, "deadbee")["l4_pass"].as_i64() != Some(0) {
        return Err("envelope score: a skipped l4 node must score 0".into());
    }
    // Comparison bracket, against real files: an equal baseline must be ACCEPTED
    // and a higher baseline must be reported as a REGRESSION.
    let dir = std::env::temp_dir().join(format!("validate-envelope-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("self-test: {e}"))?;
    let equal = dir.join("equal.json");
    let higher = dir.join("higher.json");
    // The ordered serializer must round-trip: if it dropped or renamed a key,
    // an "equal" baseline would read as 0 and every count would look improved.
    let ordered = to_ordered_json(&full);
    if !ordered.starts_with(r#"{"l1_pass":"#) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(format!("envelope JSON must start with l1_pass (validate.sh order), got {ordered:.40}"));
    }
    let round: serde_json::Value = serde_json::from_str(&ordered)
        .map_err(|e| format!("envelope JSON is not valid JSON: {e}"))?;
    for k in ["l1_pass", "l2_pass", "l3_pass", "l4_pass", "rr_pass", "total", "commit", "l4_reps", "probes"] {
        if round.get(k).is_none() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("envelope JSON lost key {k}"));
        }
    }
    std::fs::write(&equal, &ordered).map_err(|e| format!("self-test: {e}"))?;
    std::fs::write(&higher, r#"{"l1_pass":99,"l2_pass":0,"l3_pass":0,"l4_pass":0,"rr_pass":0,"total":0}"#)
        .map_err(|e| format!("self-test: {e}"))?;
    let mut accepted = 0usize;
    let mut refused = 0usize;
    match compare(&full, &equal) {
        Ok(false) => accepted += 1,
        Ok(true) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err("envelope-compare: an equal baseline must not report a regression".into());
        }
        Err((_, e)) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("envelope-compare: equal baseline errored: {e}"));
        }
    }
    match compare(&full, &higher) {
        Ok(true) => refused += 1,
        _ => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err("envelope-compare: a higher baseline MUST report a regression".into());
        }
    }
    // A missing baseline is exit 2, not a silent pass.
    let missing = dir.join("does-not-exist.json");
    if compare(&full, &missing).is_ok() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err("envelope-compare: a missing baseline must be refused".into());
    }
    refused += 1;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(format!(
        "envelope: {} nodes ({} probes x {} levels) all capped, l4->l2 edges present, \
         scoring bracketed 1 full / 1 skip-zero, comparison bracketed {accepted} accept / {refused} refuse",
        steps.len(),
        PROBES.len(),
        LEVELS.len()
    ))
}
