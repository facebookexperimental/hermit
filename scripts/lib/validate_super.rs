// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Generator and reporting support for the committed super population.
//!
//! Static super nodes are authored directly in `ci/dag/validate.json`. The
//! maintenance generator calls this module only for the namespaced `superstress`
//! partition; runtime validation selects the committed `super` label.

use std::path::Path;

use dagrun::model::Step;
use dagrun::model::StepOutcome;

use crate::validate_plan::node;
use crate::validate_plan::shell_quote;

/// `GATE_TIMEOUT_SECONDS` (validate.sh:400). A generated super node without
/// an explicit override inherits this historical default before it is committed.
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

/// One row in the mechanically extracted super source table. These rows are a
/// maintenance-time input and self-test oracle; runtime consumes only the
/// committed validation DAG.
#[derive(Clone, Debug)]
struct SuperGate {
    job: String,
    label: String,
    timeout: i64,
    argv: Vec<String>,
    synthetic: Option<String>,
}

fn apply_innermost_timeout_runner(argv: &mut Vec<String>, root: &str) -> bool {
    let Some(cargo) = argv.windows(2).position(|words| words == ["cargo", "test"]) else {
        return false;
    };
    argv[cargo] = format!("{root}/ci/run-nextest-counted.sh");
    argv.remove(cargo + 1);
    let mut jobs = None;
    let mut no_capture = false;
    argv.retain(|argument| {
        if let Some(value) = argument.strip_prefix("--test-threads=") {
            jobs = Some(value.to_string());
            false
        } else if argument == "--nocapture" {
            no_capture = true;
            false
        } else {
            true
        }
    });
    let split = argv.iter().position(|argument| argument == "--").unwrap_or(argv.len());
    if let Some(jobs) = jobs {
        argv.splice(split..split, ["-j".to_string(), jobs]);
    }
    if no_capture {
        let split = argv.iter().position(|argument| argument == "--").unwrap_or(argv.len());
        argv.insert(split, "--no-capture".to_string());
    }
    true
}

fn load_gates(root: &Path) -> Result<Vec<SuperGate>, String> {
    let file = root.join("ci/super/gates.json");
    let text = std::fs::read_to_string(&file)
        .map_err(|error| format!("cannot read super gate table {}: {error}", file.display()))?;
    let document: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("invalid JSON in {}: {error}", file.display()))?;
    let rows = document
        .get("rows")
        .and_then(|rows| rows.as_array())
        .ok_or_else(|| format!("{} has no `rows` array", file.display()))?;
    let root = root.to_string_lossy();
    let mut gates = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let string = |field: &str| {
            row.get(field)
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };
        let job = string("job")
            .ok_or_else(|| format!("{} row {index}: missing string `job`", file.display()))?;
        let label = string("label")
            .ok_or_else(|| format!("{} row {index}: missing string `label`", file.display()))?;
        let timeout = row.get("timeout").and_then(|value| value.as_i64()).unwrap_or(0);
        let raw = row
            .get("argv")
            .and_then(|value| value.as_array())
            .ok_or_else(|| format!("{} row {index} ({label}): missing array `argv`", file.display()))?;
        let mut argv = Vec::with_capacity(raw.len());
        for argument in raw {
            let argument = argument.as_str().ok_or_else(|| {
                format!("{} row {index} ({label}): non-string argv element", file.display())
            })?;
            argv.push(argument.replace("{{ROOT_DIR}}", &root));
        }
        apply_innermost_timeout_runner(&mut argv, &root);
        let synthetic = string("synthetic");
        if synthetic.is_none() && argv.is_empty() {
            return Err(format!("{} row {index} ({label}): empty argv", file.display()));
        }
        gates.push(SuperGate {
            job,
            label,
            timeout,
            argv,
            synthetic,
        });
    }
    if gates.is_empty() {
        return Err(format!("{} contained zero rows", file.display()));
    }
    Ok(gates)
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
            StressProbe::PtraceStrictVerify
            | StressProbe::PtracePipeline
            | StressProbe::PtraceRecordReplay => None,
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

// ----------------------------------------------------------------- self-test

/// Focused controls for the reporting policy that remains live after the
/// committed DAG became the sole source of super-plan construction.
pub fn self_test(root: &Path) -> Result<String, String> {
    let gates = load_gates(root)?;
    if gates.len() != 32 {
        return Err(format!(
            "super source table has {} rows; the mechanical extraction requires exactly 32",
            gates.len()
        ));
    }
    let synthetic = gates
        .iter()
        .filter_map(|gate| gate.synthetic.as_deref())
        .collect::<Vec<_>>();
    let expected_synthetic = [
        "portable_slow_strict_diagnostics",
        "super_stress_suite",
        "calibrated_analyze_tests",
    ];
    if synthetic.len() != expected_synthetic.len()
        || expected_synthetic
            .iter()
            .any(|name| !synthetic.contains(name))
    {
        return Err(format!(
            "super source table synthetic rows changed: {synthetic:?}"
        ));
    }
    let nextest_rows = gates
        .iter()
        .filter(|gate| {
            gate.argv
                .iter()
                .any(|argument| argument.ends_with("/ci/run-nextest-counted.sh"))
        })
        .count();
    if nextest_rows < 20
        || gates
            .iter()
            .any(|gate| gate.argv.windows(2).any(|words| words == ["cargo", "test"]))
    {
        return Err(format!(
            "super source table cargo-test conversion is incomplete: {nextest_rows} nextest rows"
        ));
    }
    let calibrated = gates
        .iter()
        .find(|gate| gate.synthetic.as_deref() == Some("calibrated_analyze_tests"))
        .ok_or_else(|| "super source table lost calibrated_analyze_tests".to_string())?;
    let normalized_calibrated = calibrated
        .argv
        .iter()
        .filter(|argument| !argument.starts_with("--test-threads="))
        .cloned()
        .collect::<Vec<_>>();
    if normalized_calibrated
        .iter()
        .any(|argument| argument.starts_with("--test-threads="))
        || normalized_calibrated.len() + 1 != calibrated.argv.len()
        || calibrated.job.is_empty()
        || calibrated.label.is_empty()
        || calibrated.timeout < 0
    {
        return Err(format!(
            "calibrated analyze source arguments were not normalized: {:?}",
            calibrated.argv
        ));
    }
    let committed = std::fs::read_to_string(root.join("ci/dag/validate.json"))
        .map_err(|error| format!("cannot read committed super graph: {error}"))?;
    let committed = dagrun::io::dag_from_json(&committed)
        .map_err(|error| format!("cannot parse committed super graph: {error}"))?;
    let emitted = committed
        .steps
        .iter()
        .find(|step| step.tag() == "super.pmu_analyze_hello_race_stress_calibrated_skid")
        .ok_or_else(|| "committed graph lost calibrated analyze node".to_string())?;
    if !emitted.cmd.contains("./ci/run-nextest-counted.sh")
        || emitted.cmd.contains("--test-threads=")
    {
        return Err(format!(
            "committed calibrated analyze command was not normalized: {}",
            emitted.cmd
        ));
    }

    let bad_root = std::env::temp_dir().join(format!(
        "validate-super-source-self-test-{}",
        std::process::id()
    ));
    let bad_file = bad_root.join("ci/super/gates.json");
    std::fs::create_dir_all(bad_file.parent().expect("fixture has a parent"))
        .map_err(|error| format!("cannot create super negative fixture: {error}"))?;
    let mut refused = 0;
    for (description, body) in [
        ("no rows array", r#"{"rows": {}}"#),
        ("empty rows", r#"{"rows": []}"#),
        ("row without argv", r#"{"rows":[{"job":"j","label":"l","timeout":0}]}"#),
        ("non-string argv", r#"{"rows":[{"job":"j","label":"l","argv":[7]}]}"#),
    ] {
        std::fs::write(&bad_file, body)
            .map_err(|error| format!("cannot write super negative fixture: {error}"))?;
        if load_gates(&bad_root).is_ok() {
            let _ = std::fs::remove_dir_all(&bad_root);
            return Err(format!("super source loader accepted {description}"));
        }
        refused += 1;
    }
    let _ = std::fs::remove_dir_all(&bad_root);

    let reps = 2;
    let all_green = STRESS_PROBES
        .iter()
        .copied()
        .map(|probe| ProbeRate {
            probe,
            passed: reps as usize,
            ran: reps as usize,
            planned: reps as usize,
        })
        .collect::<Vec<_>>();
    if stress_verdict(&all_green, reps, 1, 1) != 0 {
        return Err("super stress verdict: an all-passing population must be accepted".into());
    }

    let mut ptrace_miss = all_green.clone();
    ptrace_miss[0].passed -= 1;
    if stress_verdict(&ptrace_miss, reps, 1, 1) != 1 {
        return Err("super stress verdict: a ptrace miss must remain blocking".into());
    }

    let mut kvm_miss = all_green;
    let kvm = kvm_miss
        .iter_mut()
        .find(|rate| rate.probe == StressProbe::KvmVerify)
        .ok_or_else(|| "super stress verdict: KVM control is absent".to_string())?;
    kvm.passed -= 1;
    if stress_verdict(&kvm_miss, reps, 1, 1) != 0 {
        return Err("super stress verdict: the first KVM measurement must remain nonblocking".into());
    }

    Ok(format!(
        "super source: 32 rows, 3 synthetic expansions, {nextest_rows} nextest rows, {refused} malformed tables refused; stress verdict bracketed"
    ))
}

/// Environment overrides this module honors, for the plan banner.
pub fn repetitions() -> i64 {
    std::env::var("SUPER_REPETITIONS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(SUPER_REPETITIONS_DEFAULT)
}
