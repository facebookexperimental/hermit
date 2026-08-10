// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! The `super` stress/diagnostic suite, as boxed DAG nodes.
//!
//! # Where the gate table came from (provenance matters here too)
//!
//! `ci/super/gates.json` was NOT hand-transcribed from `validate.sh`. It was
//! lifted MECHANICALLY by the same technique that produced the compatibility
//! corpora: a COPY of `validate.sh` had `run_check` / `run_check_with_timeout`
//! replaced by a recorder that dumped `(timeout, label, argv)` instead of
//! executing, *after* the real bash had already evaluated every variable
//! expansion, mode gate, and conditional; `run_super_suite` was then called. The
//! dump reflects the code path, not a reading of it. 32 rows came out — 29 plain
//! commands plus three bash FUNCTIONS (`run_portable_slow_strict_diagnostics`,
//! `run_super_stress_suite`, `run_calibrated_analyze_tests`) which this module
//! expands into their own boxed nodes.
//!
//! # Two deliberate, documented departures from the bash
//!
//! **1. The KVM and DBT stress probes were DEAD CODE in `validate.sh`.**
//! `run_super_stress_suite` guards them with
//! `if backend_selector_supported && kvm_backend_available`, and
//! `backend_selector_supported` **is not defined anywhere in the repository**
//! (`validate.sh` calls it at :2694 and :2701 and defines it nowhere). An
//! undefined command exits 127, so the condition was ALWAYS false and both
//! probes have always been skipped. The port implements the availability check
//! the bash clearly intended, but expresses it as a DAG node the probe rows
//! DEPEND on: when the backend is unavailable the availability node fails and
//! its 20 dependents are *skipped*, which the runner reports as skipped rather
//! than failed — structurally the same "SKIP" the bash printed. And because
//! these rows have never been measured, [`stress_verdict`] classifies KVM/DBT
//! stress failures as NONBLOCKING and says so: a first-ever measurement must
//! arrive as data, not as an unratcheted gate that turns the suite red.
//!
//! **2. Concurrency is the scheduler's, not a hand-rolled batch loop.**
//! `run_super_probe` forked `SUPER_JOBS` copies at a time; on this 316-core box
//! `SUPER_JOBS` resolves to `(316*3+1)/2 = 474`, i.e. all 20 repetitions of a
//! probe ran at once, unboxed. Here each `(probe, iteration)` is its own DAG
//! node with its own wall/CPU/memory box, scheduled at the driver's `-j` width.

use std::path::Path;

use safe_ci_dag_runner::model::Step;
use safe_ci_dag_runner::model::StepOutcome;

use crate::validate_plan::node;
use crate::validate_plan::shell_join;
use crate::validate_plan::shell_quote;

/// `GATE_TIMEOUT_SECONDS` (validate.sh:400). A gates.json row with
/// `"timeout": 0` used bare `run_check`, which inherits this default.
pub const DEFAULT_GATE_TIMEOUT_S: i64 = 600;

/// `SUPER_REPETITIONS` (validate.sh:682).
pub const SUPER_REPETITIONS_DEFAULT: i64 = 20;

/// `STRICT_COMPAT_TIMEOUT` (validate.sh:1091) — the per-probe wall bound the
/// bash imposed with the `timeout` binary. Here it is the node's wall cap, so a
/// hung repetition is killed and reported by the runner rather than by a nested
/// `timeout` whose exit code the runner would have to reinterpret.
pub const SUPER_PROBE_TIMEOUT_S: i64 = 60;

/// CPU budget for one stress repetition. These are sub-second guest runs; a CPU
/// cap is what catches a spin that the wall cap would only catch at 60s.
const SUPER_PROBE_CPU_TIMEOUT_S: i64 = 120;
const SUPER_PROBE_MEM_BYTES: i64 = 4 * 1024 * 1024 * 1024;

/// Memory ceiling for a `cargo build` node in this suite.
const BUILD_MEM_BYTES: i64 = 16 * 1024 * 1024 * 1024;
/// Memory ceiling for a `cargo test` diagnostic node.
const TEST_MEM_BYTES: i64 = 8 * 1024 * 1024 * 1024;

/// One row of the mechanically extracted gate table.
#[derive(Clone, Debug)]
pub struct SuperGate {
    pub job: String,
    pub label: String,
    /// Wall seconds; `0` means "validate.sh's `run_check` default".
    pub timeout: i64,
    pub argv: Vec<String>,
    /// Set when the row was a bash FUNCTION rather than a command.
    pub synthetic: Option<String>,
}

impl SuperGate {
    /// Resolved wall budget: the row's own, or the `run_check` default.
    pub fn wall(&self) -> i64 {
        if self.timeout > 0 { self.timeout } else { DEFAULT_GATE_TIMEOUT_S }
    }
}

/// Load `ci/super/gates.json`.
///
/// Fails LOUDLY on an empty or malformed table for the same reason
/// `validate_corpus::load` does: a silently-empty super suite would report
/// "pass" having run nothing, which is the zero-executed-tests defect class.
pub fn load_gates(root: &Path) -> Result<Vec<SuperGate>, String> {
    let file = root.join("ci").join("super").join("gates.json");
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("cannot read super gate table {}: {e}", file.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("invalid JSON in {}: {e}", file.display()))?;
    let rows = doc
        .get("rows")
        .and_then(|r| r.as_array())
        .ok_or_else(|| format!("{} has no `rows` array", file.display()))?;
    let root_s = root.to_string_lossy().to_string();
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let get_str = |k: &str| row.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
        let job = get_str("job")
            .ok_or_else(|| format!("{} row {i}: missing string `job`", file.display()))?;
        let label = get_str("label")
            .ok_or_else(|| format!("{} row {i}: missing string `label`", file.display()))?;
        let timeout = row.get("timeout").and_then(|v| v.as_i64()).unwrap_or(0);
        let argv_raw = row
            .get("argv")
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("{} row {i} ({label}): missing array `argv`", file.display()))?;
        let mut argv = Vec::with_capacity(argv_raw.len());
        for a in argv_raw {
            let s = a.as_str().ok_or_else(|| {
                format!("{} row {i} ({label}): non-string argv element", file.display())
            })?;
            argv.push(s.replace("{{ROOT_DIR}}", &root_s));
        }
        let synthetic = get_str("synthetic");
        if synthetic.is_none() && argv.is_empty() {
            return Err(format!("{} row {i} ({label}): empty argv", file.display()));
        }
        out.push(SuperGate { job, label, timeout, argv, synthetic });
    }
    if out.is_empty() {
        return Err(format!("{} contained zero rows", file.display()));
    }
    Ok(out)
}

/// Memory hint for a plain gate row, chosen from what the command actually is.
fn mem_for(argv: &[String]) -> i64 {
    let joined = argv.join(" ");
    if joined.contains("cargo build") || joined.contains("prepare_leveldb") {
        BUILD_MEM_BYTES
    } else {
        TEST_MEM_BYTES
    }
}

/// Build a plain (non-synthetic) super gate node.
pub fn gate_node(g: &SuperGate, deps: Vec<String>) -> Step {
    let wall = g.wall();
    node(
        "super",
        &g.job,
        &g.label,
        shell_join(&g.argv),
        deps,
        wall,
        (wall * 2).min(7200),
        mem_for(&g.argv),
    )
}

// --------------------------------------------------------------------- stress

/// The five probes `run_super_stress_suite` names (validate.sh:2686, :2695, :2702).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StressProbe {
    PtraceStrictVerify,
    PtracePipeline,
    PtraceRecordReplay,
    KvmVerify,
    DbtVerify,
}

impl StressProbe {
    pub fn slug(self) -> &'static str {
        match self {
            StressProbe::PtraceStrictVerify => "ptrace-strict-verify",
            StressProbe::PtracePipeline => "ptrace-pipeline",
            StressProbe::PtraceRecordReplay => "ptrace-record-replay",
            StressProbe::KvmVerify => "kvm-verify",
            StressProbe::DbtVerify => "dbt-verify",
        }
    }

    fn job_stem(self) -> String {
        self.slug().replace('-', "_")
    }

    /// The availability node this probe depends on, if any.
    fn availability_job(self) -> Option<&'static str> {
        match self {
            StressProbe::KvmVerify => Some("kvm_available"),
            StressProbe::DbtVerify => Some("dbt_available"),
            _ => None,
        }
    }

    /// True when a failure of this probe must NOT turn the suite red.
    ///
    /// See the module doc: `backend_selector_supported` is undefined, so KVM and
    /// DBT stress have never actually been measured by `validate.sh`. Their
    /// first measurement is reported, not ratcheted.
    pub fn nonblocking(self) -> bool {
        matches!(self, StressProbe::KvmVerify | StressProbe::DbtVerify)
    }

    /// One repetition's shell command, reproducing `super_probe_command`
    /// (validate.sh:2589). The outer `timeout` binary is dropped because the
    /// node's own wall cap enforces the same bound and the runner then reports a
    /// TYPED timeout instead of an opaque exit 124.
    fn command(self, iteration: i64, release_bin: &str, debug_bin: &str, tmp: &Path) -> String {
        let rel = shell_quote(release_bin);
        let dbg = shell_quote(debug_bin);
        match self {
            StressProbe::PtraceStrictVerify => format!(
                "{rel} run --strict --verify -- /bin/echo hermit-super-{iteration} </dev/null"
            ),
            StressProbe::PtracePipeline => format!(
                "{rel} run --strict --verify -- bash -c 'yes hermit | head -n 64 | sha256sum' </dev/null"
            ),
            StressProbe::PtraceRecordReplay => {
                let dir = shell_quote(
                    &tmp.join(format!("super-record-{iteration}")).to_string_lossy(),
                );
                // The bash removed the data dir before AND after, preserving the
                // record phase's exit status across the second removal.
                format!(
                    "rm -rf {dir}; {rel} record start --verify --data-dir {dir} -- \
                     /bin/echo hermit-super-record-{iteration} </dev/null; \
                     status=$?; rm -rf {dir}; exit $status"
                )
            }
            StressProbe::KvmVerify => format!(
                "{dbg} run --backend kvm --verify -- /bin/echo hermit-super-kvm-{iteration} </dev/null"
            ),
            StressProbe::DbtVerify => format!(
                "{dbg} run --backend dbt --verify -- /bin/echo hermit-super-dbt-{iteration} </dev/null"
            ),
        }
    }
}

pub const STRESS_PROBES: &[StressProbe] = &[
    StressProbe::PtraceStrictVerify,
    StressProbe::PtracePipeline,
    StressProbe::PtraceRecordReplay,
    StressProbe::KvmVerify,
    StressProbe::DbtVerify,
];

/// The two backend-availability nodes.
///
/// `kvm_backend_available` (validate.sh:2272) is a readable+writable `/dev/kvm`;
/// `dbt_backend_available` (validate.sh:2276) is a real probe run, which is why
/// it must be a node — at plan time the debug binary does not exist yet.
fn availability_nodes(debug_bin: &str, build_dep: &str) -> Vec<Step> {
    let dbg = shell_quote(debug_bin);
    vec![
        node(
            "superstress",
            "kvm_available",
            "KVM backend availability (gates the KVM stress rows)",
            "test -r /dev/kvm && test -w /dev/kvm".to_string(),
            vec![build_dep.to_string()],
            30,
            30,
            256 * 1024 * 1024,
        ),
        node(
            "superstress",
            "dbt_available",
            "DBT backend availability (gates the DBT stress rows)",
            format!(
                "{dbg} --log=info run --backend dbt --strict --verify -- \
                 /bin/echo hermit-dbt-probe </dev/null >/dev/null 2>&1"
            ),
            vec![build_dep.to_string()],
            60,
            120,
            SUPER_PROBE_MEM_BYTES,
        ),
    ]
}

/// Build every stress node: two availability probes plus `reps` repetitions of
/// each of the five probes.
pub fn stress_nodes(
    release_bin: &str,
    debug_bin: &str,
    tmp: &Path,
    reps: i64,
    release_dep: &str,
    debug_dep: &str,
) -> Vec<Step> {
    let mut out = availability_nodes(debug_bin, debug_dep);
    for probe in STRESS_PROBES {
        let stem = probe.job_stem();
        let base_dep = match probe {
            StressProbe::KvmVerify | StressProbe::DbtVerify => debug_dep,
            _ => release_dep,
        };
        let mut deps = vec![base_dep.to_string()];
        if let Some(av) = probe.availability_job() {
            deps.push(format!("superstress.{av}"));
        }
        for i in 1..=reps {
            out.push(node(
                "superstress",
                &format!("{stem}_{i:02}"),
                &format!("super stress {} repetition {i}/{reps}", probe.slug()),
                probe.command(i, release_bin, debug_bin, tmp),
                deps.clone(),
                SUPER_PROBE_TIMEOUT_S,
                SUPER_PROBE_CPU_TIMEOUT_S,
                SUPER_PROBE_MEM_BYTES,
            ));
        }
    }
    out
}

/// Per-probe pass rate, derived from typed outcomes.
#[derive(Clone, Debug)]
pub struct ProbeRate {
    pub probe: StressProbe,
    pub passed: usize,
    /// Repetitions that actually ran (a skipped dependent never ran).
    pub ran: usize,
    pub planned: usize,
}

/// Recompute `run_super_probe`'s report from typed `StepOutcome`s.
///
/// The bash scraped its own tee'd text file (`$VALIDATION_TMP_DIR/super-report`);
/// this reads the runner's structured verdicts, so the printed rate and the
/// blocking decision cannot disagree with what actually ran.
pub fn stress_rates(outcomes: &[StepOutcome], reps: i64) -> Vec<ProbeRate> {
    let mut rates = Vec::new();
    for probe in STRESS_PROBES {
        let stem = probe.job_stem();
        let prefix = format!("superstress.{stem}_");
        let mut passed = 0usize;
        let mut ran = 0usize;
        for o in outcomes {
            if !o.tag.starts_with(&prefix) {
                continue;
            }
            if o.aborted {
                continue;
            }
            ran += 1;
            if o.ok {
                passed += 1;
            }
        }
        rates.push(ProbeRate { probe: *probe, passed, ran, planned: reps as usize });
    }
    rates
}

/// Print the pass-rate table and return the BLOCKING failure count.
///
/// A probe is blocking iff it is a ptrace probe (the three the bash actually
/// measured) and it did not pass every planned repetition. KVM/DBT rates are
/// printed with the reason they are nonblocking, so the number is visible
/// without silently becoming a gate on its first appearance.
pub fn stress_verdict(rates: &[ProbeRate], reps: i64, jobs: i64, host_cpus: usize) -> usize {
    println!("\n== Super stress pass rates ==");
    println!("Repetitions: {reps}; scheduler width: {jobs}; online CPUs: {host_cpus}");
    let mut blocking = 0usize;
    for r in rates {
        let slug = r.probe.slug();
        if r.ran == 0 {
            println!("  SKIP {slug:<24} backend unavailable (availability node failed; 0/{reps} ran)");
            continue;
        }
        let pct = 100 * r.passed / r.planned.max(1);
        if r.passed == r.planned {
            println!("  ✅ {slug:<24} {}/{} (100%)", r.passed, r.planned);
        } else if r.probe.nonblocking() {
            println!(
                "  ⚠️  {slug:<24} {}/{} ({pct}%) FLAKY/FAILING — NONBLOCKING: this row was dead \
                 code in validate.sh (`backend_selector_supported` is undefined, so the guard was \
                 always false) and has never been measured; reporting it, not ratcheting it.",
                r.passed, r.planned
            );
        } else {
            println!("  ⚠️  {slug:<24} {}/{} ({pct}%) FLAKY/FAILING", r.passed, r.planned);
            blocking += 1;
        }
    }
    blocking
}

/// Tags of every stress node whose failure must not turn the suite red.
pub fn nonblocking_tags(reps: i64) -> Vec<String> {
    let mut out = vec!["superstress.kvm_available".to_string(), "superstress.dbt_available".to_string()];
    for probe in STRESS_PROBES.iter().filter(|p| p.nonblocking()) {
        let stem = probe.job_stem();
        for i in 1..=reps {
            out.push(format!("superstress.{stem}_{i:02}"));
        }
    }
    out
}

// ------------------------------------------------------- calibrated analyze

/// `run_calibrated_analyze_tests` (validate.sh:4526) as one boxed node.
///
/// Kept as a single node because the three steps are one measurement: build the
/// skid probe, read its recommended RCB margin, then run the analyze test with
/// that margin exported. Splitting them would require passing a value between
/// nodes, and the runner has no such channel. Every knob the bash exposed as an
/// environment override is preserved verbatim.
pub fn calibrated_analyze_node(g: &SuperGate, deps: Vec<String>) -> Step {
    let test_args = shell_join(&g.argv);
    let cmd = format!(
        r#"set -u
iters=${{ANALYZE_SKID_CALIBRATION_ITERATIONS:-64}}
period=${{ANALYZE_SKID_CALIBRATION_PERIOD:-1000000}}
floor=${{ANALYZE_SKID_MINIMUM_MARGIN:-20000}}
cal_timeout=${{ANALYZE_SKID_CALIBRATION_TIMEOUT:-30}}
for name in iters period floor cal_timeout; do
    eval "v=\$$name"
    case "$v" in
        ''|*[!0-9]*|0*) echo "Analyze PMU calibration error: $name must be a positive integer, got $v" >&2; exit 2 ;;
    esac
done
bin=$PWD/target/ci-pmu-skid
mkdir -p "$(dirname "$bin")" || exit 1
cc -O2 -Wall -Wextra -Werror -std=gnu11 tests/util/pmu_skid.c -o "$bin" || {{
    echo "Analyze PMU calibration error: failed to build tests/util/pmu_skid.c" >&2; exit 1; }}
out=$(timeout "$cal_timeout" "$bin" --iterations "$iters" --period "$period" 2>&1) || {{
    status=$?; printf 'Analyze PMU calibration failed (exit %s):\n%s\n' "$status" "$out" >&2; exit "$status"; }}
printf '%s\n' "$out"
rec=$(printf '%s\n' "$out" | sed -n 's/^Recommended margin: \([0-9][0-9]*\) RCB.*/\1/p')
case "$rec" in
    ''|*[!0-9]*|0*) echo "Analyze PMU calibration error: output omitted a valid recommended margin" >&2; exit 1 ;;
esac
margin=$rec
if [ "$margin" -lt "$floor" ]; then margin=$floor; fi
printf 'Analyze PMU skid margin: calibrated=%s RCB, conservative floor=%s RCB, using=%s RCB\n' \
    "$rec" "$floor" "$margin"
HERMIT_ANALYZE_SKID_MARGIN=$margin cargo test -p hermit --features third-party-backends --test analyze {test_args}"#
    );
    let wall = g.wall();
    node("super", &g.job, &g.label, cmd, deps, wall, (wall * 2).min(7200), TEST_MEM_BYTES)
}

// ----------------------------------------------------------------- self-test

/// Inert brackets for this module. Neither branch can run a gate: they only
/// construct nodes and classify synthetic outcomes.
pub fn self_test(root: &Path) -> Result<String, String> {
    let gates = load_gates(root)?;
    // Positive: the qualifying table must be ACCEPTED with its three synthetic
    // rows recognized, so a table that silently lost one cannot pass.
    let synth: Vec<&str> =
        gates.iter().filter_map(|g| g.synthetic.as_deref()).collect();
    let want = ["portable_slow_strict_diagnostics", "super_stress_suite", "calibrated_analyze_tests"];
    for w in want {
        if !synth.contains(&w) {
            return Err(format!("super gate table lost its `{w}` synthetic row"));
        }
    }
    if gates.len() < 30 {
        return Err(format!(
            "super gate table has only {} rows; the mechanical extraction produced 32",
            gates.len()
        ));
    }
    // Negative: a table whose rows are not an array, or whose row lacks argv,
    // must be REFUSED rather than yielding an empty (silently green) suite.
    let bad_dir = std::env::temp_dir().join(format!("validate-super-selftest-{}", std::process::id()));
    let bad_file = bad_dir.join("ci").join("super").join("gates.json");
    std::fs::create_dir_all(bad_file.parent().unwrap())
        .map_err(|e| format!("self-test: cannot stage negative fixture: {e}"))?;
    let mut refused = 0usize;
    for (why, body) in [
        ("no rows array", r#"{"rows": {}}"#),
        ("empty rows", r#"{"rows": []}"#),
        ("row without argv", r#"{"rows":[{"job":"j","label":"l","timeout":0}]}"#),
        ("row with non-string argv", r#"{"rows":[{"job":"j","label":"l","argv":[7]}]}"#),
    ] {
        std::fs::write(&bad_file, body)
            .map_err(|e| format!("self-test: cannot write negative fixture: {e}"))?;
        if load_gates(&bad_dir).is_ok() {
            let _ = std::fs::remove_dir_all(&bad_dir);
            return Err(format!("super gate loader ACCEPTED a malformed table ({why})"));
        }
        refused += 1;
    }
    let _ = std::fs::remove_dir_all(&bad_dir);

    // Bracket the stress verdict on both sides with synthetic outcomes: a full
    // ptrace sweep must be ACCEPTED (0 blocking) and a single ptrace miss must
    // be REFUSED (1 blocking), while the same miss on KVM stays nonblocking.
    let reps = 3i64;
    let mk = |tag: &str, ok: bool| StepOutcome {
        tag: tag.to_string(),
        ok,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests: None,
        filtered_tests: None,
        returncode: Some(if ok { 0 } else { 1 }),
        reason: String::new(),
        aborted: false,
    };
    let mut all_pass = Vec::new();
    for probe in STRESS_PROBES {
        for i in 1..=reps {
            all_pass.push(mk(&format!("superstress.{}_{i:02}", probe.slug().replace('-', "_")), true));
        }
    }
    let rates = stress_rates(&all_pass, reps);
    if count_blocking(&rates) != 0 {
        return Err("stress verdict: an all-passing sweep must be accepted".into());
    }
    let mut one_ptrace_miss = all_pass.clone();
    one_ptrace_miss[0] = mk("superstress.ptrace_strict_verify_01", false);
    if count_blocking(&stress_rates(&one_ptrace_miss, reps)) != 1 {
        return Err("stress verdict: a ptrace repetition failure must be blocking".into());
    }
    let mut one_kvm_miss = all_pass.clone();
    let kvm_idx = all_pass
        .iter()
        .position(|o| o.tag.starts_with("superstress.kvm_verify_"))
        .ok_or("stress verdict: kvm rows absent from the synthetic sweep")?;
    one_kvm_miss[kvm_idx] = mk("superstress.kvm_verify_01", false);
    if count_blocking(&stress_rates(&one_kvm_miss, reps)) != 0 {
        return Err("stress verdict: a KVM repetition failure must stay NONBLOCKING".into());
    }
    Ok(format!(
        "super: {} gate rows ({} synthetic), {refused} malformed tables refused, \
         stress verdict bracketed 1 accept / 1 blocking-refusal / 1 nonblocking",
        gates.len(),
        synth.len()
    ))
}

/// Blocking count without printing (used by the inert brackets).
fn count_blocking(rates: &[ProbeRate]) -> usize {
    rates
        .iter()
        .filter(|r| r.ran > 0 && r.passed != r.planned && !r.probe.nonblocking())
        .count()
}

/// Environment overrides this module honors, for the plan banner.
pub fn repetitions() -> i64 {
    std::env::var("SUPER_REPETITIONS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(SUPER_REPETITIONS_DEFAULT)
}
