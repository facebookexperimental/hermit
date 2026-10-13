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

use dagrun::model::ResultManifest;
use dagrun::model::Step;
use dagrun::model::StepOutcome;
use dagrun::model::StructuredTestResultsManifest;

use crate::validate_plan::node;
use crate::validate_plan::shell_quote;

/// `GATE_TIMEOUT_SECONDS` (validate.sh:400). A generated super node without
/// an explicit override inherits this historical default before it is committed.
pub const DEFAULT_GATE_TIMEOUT_S: i64 = 600;

/// `SUPER_REPETITIONS` (validate.sh:682).
pub const SUPER_REPETITIONS_DEFAULT: i64 = 20;

/// `STRICT_COMPAT_TIMEOUT` (validate.sh:1091) — the per-repetition wall bound
/// the bash imposed with the `timeout` binary. All repetitions of a probe share
/// one node, so this bound is a `timeout` around each repetition's Hermit run,
/// and a repetition it kills is recorded as that repetition's failure.
pub const SUPER_PROBE_TIMEOUT_S: i64 = 60;

/// Grace between `timeout`'s SIGTERM and its SIGKILL for one repetition.
const SUPER_PROBE_KILL_GRACE_S: i64 = 10;

/// CPU budget for one stress repetition. These are sub-second guest runs; a CPU
/// cap is what catches a spin that the wall bound would only catch at 60s. The
/// node's CPU cap is this budget times the repetition count, and the node also
/// fails any single repetition whose waited CPU time reached this budget.
const SUPER_PROBE_CPU_TIMEOUT_S: i64 = 120;

/// Environment variable through which dagrun hands a probe node its admitted
/// width: the number of repetitions the node runs at the same time.
const SUPER_PROBE_JOBS_ENV: &str = "HERMIT_SUPER_STRESS_JOBS";

/// Wall slack for a probe node beyond its repetitions' bounds: the loop, the
/// record probe's data-directory removal, and the result write. The node's
/// wall cap is every repetition's bound end to end plus this slack, so it
/// still holds if dagrun admits the node at width 1.
const SUPER_PROBE_NODE_SLACK_S: i64 = 60;

/// Memory cap for one probe node, all of its concurrent repetitions included.
/// Measured 2026-10-02 on the host recorded in docs/TESTING_ENVIRONMENTS.md
/// under "Named measurement hosts", with 20 repetitions at once, each Hermit
/// call in its own safehermit cgroup: the 20 per-repetition peaks summed to
/// 1052, 1226 and 1433 MiB for the strict, pipeline and record probes (largest
/// single repetition 73 MiB), so 4 GiB is 2.8 times the largest sum. The KVM
/// probe, measured the same way with the release binary, summed to 1363 MiB
/// (largest 71 MiB). The DBT probe is unmeasured: no Hermit binary on the host
/// was built with the dbt feature. It is nonblocking, and if it reaches this
/// cap the node writes no rows, so the pass-rate table shows NO_RESULT.
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
    let split = argv
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(argv.len());
    if let Some(jobs) = jobs {
        argv.splice(split..split, ["-j".to_string(), jobs]);
    }
    if no_capture {
        let split = argv
            .iter()
            .position(|argument| argument == "--")
            .unwrap_or(argv.len());
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
        let timeout = row
            .get("timeout")
            .and_then(|value| value.as_i64())
            .unwrap_or(0);
        let raw = row
            .get("argv")
            .and_then(|value| value.as_array())
            .ok_or_else(|| {
                format!(
                    "{} row {index} ({label}): missing array `argv`",
                    file.display()
                )
            })?;
        let mut argv = Vec::with_capacity(raw.len());
        for argument in raw {
            let argument = argument.as_str().ok_or_else(|| {
                format!(
                    "{} row {index} ({label}): non-string argv element",
                    file.display()
                )
            })?;
            argv.push(argument.replace("{{ROOT_DIR}}", &root));
        }
        apply_innermost_timeout_runner(&mut argv, &root);
        let synthetic = string("synthetic");
        if synthetic.is_none() && argv.is_empty() {
            return Err(format!(
                "{} row {index} ({label}): empty argv",
                file.display()
            ));
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

    pub fn job_stem(self) -> String {
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

    /// The node's inventory paragraph.
    fn description(self, reps: i64) -> String {
        let (what, binary) = match self {
            StressProbe::PtraceStrictVerify => (
                "hermit run --strict --verify of /bin/echo on the ptrace backend",
                "the release Hermit binary",
            ),
            StressProbe::PtracePipeline => (
                "hermit run --strict --verify of a bash pipeline (yes | head -n 64 | sha256sum) \
                 on the ptrace backend",
                "the release Hermit binary",
            ),
            StressProbe::PtraceRecordReplay => (
                "hermit record start --verify of /bin/echo on the ptrace backend, recording into \
                 a per-repetition data directory under the validation temporary directory and \
                 removing it before and after",
                "the release Hermit binary",
            ),
            StressProbe::KvmVerify => (
                "hermit --backend kvm run --verify of /bin/echo",
                "the debug Hermit binary, after superstress.kvm_available finds /dev/kvm readable \
                 and writable",
            ),
            StressProbe::DbtVerify => (
                "hermit --backend dbt run --verify of /bin/echo",
                "the debug Hermit binary, after superstress.dbt_available completes one DBT run",
            ),
        };
        let policy = if self.nonblocking() {
            "Its failures are reported but do not block the super run: validate.sh never \
             measured this probe (its backend guard was always false), so its first measurements \
             are reported rather than ratcheted."
        } else {
            "Any failed repetition fails the node and blocks the super run."
        };
        format!(
            "Runs {reps} repetitions of {what}, using {binary}. The repetitions run at the same \
             time, as many at once as the node's admitted width ({SUPER_PROBE_JOBS_ENV}, declared \
             as {reps}), so the probe still loads the host the way {reps} separate nodes did. Each \
             repetition runs in its own background subshell under timeout \
             {SUPER_PROBE_TIMEOUT_S} s (SIGKILL {SUPER_PROBE_KILL_GRACE_S} s later) and fails if \
             its waited CPU time, read with the bash times builtin, reaches \
             {SUPER_PROBE_CPU_TIMEOUT_S} s. Each repetition's output goes to its own log, which \
             the node prints, indented and tagged with the repetition number, under that \
             repetition's result line; the node writes each repetition as a pass or fail row of its schema-2 structured test results, which the super \
             stress pass-rate table counts. {policy} If the node itself is killed by its wall, \
             CPU or memory cap, it writes no rows and the table shows the probe as NO_RESULT; the \
             node's failure still counts. Until 2026-10 each repetition was its own DAG node; \
             folding them into one node per probe keeps the commands, the repetition count, the \
             concurrency and the per-repetition wall and CPU bounds, and removes {} nodes from the \
             plan.",
            reps - 1
        )
    }

    /// One repetition's shell command, reproducing `super_probe_command`
    /// (validate.sh:2589). `iteration` is the shell expression naming the
    /// repetition, and `bound` prefixes the Hermit invocation with that
    /// repetition's wall bound: the repetitions share one node, so the node's
    /// own wall cap can no longer say which repetition hung.
    fn command(
        self,
        iteration: &str,
        bound: &str,
        release_bin: &str,
        debug_bin: &str,
        tmp: &Path,
    ) -> String {
        let rel = shell_quote(release_bin);
        let dbg = shell_quote(debug_bin);
        match self {
            StressProbe::PtraceStrictVerify => format!(
                "{bound}{rel} run --strict --verify -- /bin/echo hermit-super-{iteration} </dev/null"
            ),
            StressProbe::PtracePipeline => format!(
                "{bound}{rel} run --strict --verify -- bash -c 'yes hermit | head -n 64 | sha256sum' </dev/null"
            ),
            StressProbe::PtraceRecordReplay => {
                let dir = format!(
                    "{}/super-record-{iteration}",
                    shell_quote(&tmp.to_string_lossy())
                );
                // The bash removed the data dir before AND after, preserving the
                // record phase's exit status across the second removal.
                format!(
                    "rm -rf {dir}; {bound}{rel} record start --verify --data-dir {dir} -- \
                     /bin/echo hermit-super-record-{iteration} </dev/null; \
                     status=$?; rm -rf {dir}; exit $status"
                )
            }
            StressProbe::KvmVerify => format!(
                "{bound}{dbg} --backend kvm run --verify -- /bin/echo hermit-super-kvm-{iteration} </dev/null"
            ),
            StressProbe::DbtVerify => format!(
                "{bound}{dbg} --backend dbt run --verify -- /bin/echo hermit-super-dbt-{iteration} </dev/null"
            ),
        }
    }

    /// The node that runs every repetition of this probe.
    ///
    /// The separate nodes ran concurrently, so the repetitions do too: each is
    /// a background subshell under `timeout` with the per-repetition wall bound
    /// the separate nodes had, at most the admitted width at once. Each records
    /// its exit status, its waited CPU time (`times`, children line) and its
    /// output in its own files, and the node fails a repetition whose CPU time reached the
    /// per-repetition budget, which the separate nodes' CPU caps enforced. Each
    /// repetition is a row of the node's structured test results, written once
    /// by the registered writer `ci/write-structured-test-counts.sh`, so a
    /// failed repetition stays typed evidence with its own identity. The node
    /// fails when any repetition fails.
    fn node_command(self, reps: i64, release_bin: &str, debug_bin: &str, tmp: &Path) -> String {
        let slug = self.slug();
        // --verbose makes timeout say which signal it sent, in the repetition's
        // log, so the result line names a timeout only when timeout sent one: an
        // exit status of 124 or 137 alone could also be Hermit's own. The line
        // must also agree with the exit status timeout returns after sending
        // that signal, because the guest's output shares the log. The outer
        // subshell writes to the log too, so bash's report of a killed job is
        // tagged with its repetition rather than left untagged on stderr.
        let bound = format!(
            "timeout --verbose --kill-after={SUPER_PROBE_KILL_GRACE_S} {SUPER_PROBE_TIMEOUT_S} "
        );
        let repetition = self.command("$rep", &bound, release_bin, debug_bin, tmp);
        let jobs_env = SUPER_PROBE_JOBS_ENV;
        format!(
            ": \"${{DAGRUN_TEST_COUNTS_PATH:?the structured result path is unset}}\"; \
             width=${{{jobs_env}:-{reps}}}; \
             case $width in ''|*[!0-9]*) echo \"super stress {slug}: {jobs_env}=$width is not a whole number\" >&2; exit 2;; esac; \
             [ \"$width\" -ge 1 ] || width=1; [ \"$width\" -le {reps} ] || width={reps}; \
             state=$(mktemp -d) || exit 2; trap 'rm -rf \"$state\"' EXIT; \
             running=0; \
             for rep in $(seq 1 {reps}); do \
             if [ \"$running\" -ge \"$width\" ]; then wait -n; running=$((running - 1)); fi; \
             ( ( {repetition} ); rc=$?; times >\"$state/$rep.times\"; echo \"$rc\" >\"$state/$rep.rc\" ) >\"$state/$rep.log\" 2>&1 & \
             running=$((running + 1)); \
             done; \
             wait; \
             rows=(); failed=0; \
             for rep in $(seq 1 {reps}); do \
             rc=$(cat \"$state/$rep.rc\" 2>/dev/null); rc=${{rc:-none}}; \
             cpu=$(awk 'NR == 2 {{ gsub(/s/, \"\"); split($1, u, \"m\"); split($2, k, \"m\"); printf \"%.2f\", u[1] * 60 + u[2] + k[1] * 60 + k[2] }}' \"$state/$rep.times\" 2>/dev/null); cpu=${{cpu:-0}}; \
             log=\"$state/$rep.log\"; \
             if [ \"$rc\" = 137 ] && grep -q 'timeout: sending signal KILL' \"$log\" 2>/dev/null; then cause=' (timeout sent SIGKILL {SUPER_PROBE_KILL_GRACE_S} s after the {SUPER_PROBE_TIMEOUT_S} s bound)'; \
             elif [ \"$rc\" = 124 ] && grep -q 'timeout: sending signal TERM' \"$log\" 2>/dev/null; then cause=' (timed out after {SUPER_PROBE_TIMEOUT_S} s)'; \
             else case $rc in \
             none) cause=' (wrote no exit status)';; \
             137) cause=' (SIGKILL not sent by its timeout, e.g. the OOM killer)';; \
             *) cause=;; \
             esac; fi; \
             result=pass; [ \"$rc\" = 0 ] || result=fail; \
             if [ \"${{cpu%.*}}\" -ge {SUPER_PROBE_CPU_TIMEOUT_S} ]; then result=fail; cause=\"$cause (used $cpu CPU seconds; the per-repetition bound is {SUPER_PROBE_CPU_TIMEOUT_S})\"; fi; \
             [ \"$result\" = pass ] || failed=$((failed + 1)); \
             echo \"super stress {slug} repetition $rep/{reps}: $result, exit $rc, $cpu CPU s$cause\"; \
             awk -v r=\"$rep\" '{{ print \"  [\" r \"] \" $0 }}' \"$log\" 2>/dev/null; \
             rows+=(\"$(printf '{slug}/repetition-%02d' \"$rep\")\" \"$result\" 1); \
             done; \
             ./ci/write-structured-test-counts.sh {reps} 0 \"${{rows[@]}}\" || exit 2; \
             echo \"super stress {slug}: $(({reps} - failed))/{reps} repetitions passed, up to $width at a time\"; \
             [ \"$failed\" -eq 0 ]"
        )
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
fn availability_nodes(debug_bin: &str, build_dep: &str, reps: i64) -> Vec<Step> {
    let dbg = shell_quote(debug_bin);
    vec![
        described(
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
            format!(
                "Runs `test -r /dev/kvm && test -w /dev/kvm` after {build_dep}, the port of \
                 validate.sh's kvm_backend_available. Its only dependent is superstress.kvm_verify, \
                 which runs {reps} repetitions of `hermit --backend kvm run --verify` of /bin/echo \
                 with the debug binary; when this node fails, that node is skipped and the super \
                 stress table prints \"SKIP kvm-verify backend unavailable (availability node \
                 failed; 0/{reps} ran)\". Like the KVM stress node, it is nonblocking in the super \
                 profile, so a failure is reported but does not turn the run red. A host or container \
                 without /dev/kvm, or one where the validating user cannot open it read-write, makes \
                 it fail."
            ),
        ),
        described(
            node(
                "superstress",
                "dbt_available",
                "DBT backend availability (gates the DBT stress rows)",
                format!(
                    "{dbg} --log=info --backend dbt run --strict --verify -- \
                 /bin/echo hermit-dbt-probe </dev/null >/dev/null 2>&1"
                ),
                vec![build_dep.to_string()],
                60,
                120,
                SUPER_PROBE_MEM_BYTES,
            ),
            format!(
                "Runs one real DBT run with the debug Hermit after {build_dep}: `hermit --log=info \
             --backend dbt run --strict --verify -- /bin/echo hermit-dbt-probe`, the port of \
                 validate.sh's dbt_backend_available. It is a node because the debug binary does not \
                 exist when the plan is built. Its only dependent is superstress.dbt_verify, {reps} \
                 repetitions of `hermit --backend dbt run --verify` of /bin/echo; when this probe \
                 fails, that node is skipped and the super stress table prints \"SKIP dbt-verify \
                 backend unavailable\". Both nodes are nonblocking in the super profile, so their \
                 failures are reported, not gated. A strict-mode refusal or a divergence between the \
                 two runs makes it exit nonzero; its output goes to /dev/null, so the node log does \
                 not say which."
            ),
        ),
    ]
}

/// Attach an inventory paragraph to a node built by [`node`].
fn described(mut step: Step, description: String) -> Step {
    step.description = description;
    step
}

/// Build every stress node: two availability probes plus one node per probe
/// that runs its `reps` repetitions.
pub fn stress_nodes(
    release_bin: &str,
    debug_bin: &str,
    tmp: &Path,
    reps: i64,
    release_dep: &str,
    debug_dep: &str,
) -> Vec<Step> {
    let mut out = availability_nodes(debug_bin, debug_dep, reps);
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
        let per_repetition = SUPER_PROBE_TIMEOUT_S + SUPER_PROBE_KILL_GRACE_S;
        let mut step = node(
            "superstress",
            &stem,
            &format!("super stress {} ({reps} repetitions)", probe.slug()),
            probe.node_command(reps, release_bin, debug_bin, tmp),
            deps,
            reps * per_repetition + SUPER_PROBE_NODE_SLACK_S,
            reps * SUPER_PROBE_CPU_TIMEOUT_S,
            SUPER_PROBE_MEM_BYTES,
        );
        // Declare the width the repetitions run at, so dagrun reserves (and
        // its cpu.max allows) that many cores. jobs_flag is empty: the width
        // reaches the command through SUPER_PROBE_JOBS_ENV, never a `-j`.
        step.hint.preferred_inner_jobs = Some(reps);
        step.jobs_env = Some(SUPER_PROBE_JOBS_ENV.to_string());
        step.jobs_flag = Some(String::new());
        step.description = probe.description(reps);
        step.result_manifests = Some(vec![ResultManifest::StructuredTestResults(
            StructuredTestResultsManifest::current(format!("superstress.{stem}")),
        )]);
        out.push(step);
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
        let tag = format!("superstress.{}", probe.job_stem());
        // A repetition ran when the probe node reported a result row for it. A
        // node that was skipped, aborted, or exited without its result file ran
        // none, which the verdict reports as NO_RESULT rather than as passes.
        let rows = outcomes
            .iter()
            .filter(|o| o.tag == tag && !o.aborted)
            .filter_map(|o| o.test_results.as_ref())
            .flatten();
        let mut passed = 0usize;
        let mut ran = 0usize;
        for row in rows {
            ran += 1;
            if row.passed {
                passed += 1;
            }
        }
        rates.push(ProbeRate {
            probe: *probe,
            passed,
            ran,
            planned: reps as usize,
        });
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
            println!(
                "  SKIP {slug:<24} backend unavailable (availability node failed; 0/{reps} ran)"
            );
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
            println!(
                "  ⚠️  {slug:<24} {}/{} ({pct}%) FLAKY/FAILING",
                r.passed, r.planned
            );
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
    // 31 since the liteinst_python3_verify_diagnostics row left with the
    // ptrace-hosted LiteInst hybrid (https://github.com/rrnewton/hermit/issues/3520);
    // it had already stopped selecting any test.
    if gates.len() != 31 {
        return Err(format!(
            "super source table has {} rows; the mechanical extraction requires exactly 31",
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
        (
            "row without argv",
            r#"{"rows":[{"job":"j","label":"l","timeout":0}]}"#,
        ),
        (
            "non-string argv",
            r#"{"rows":[{"job":"j","label":"l","argv":[7]}]}"#,
        ),
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
        return Err(
            "super stress verdict: the first KVM measurement must remain nonblocking".into(),
        );
    }

    let standin = stress_standin_bracket(root)?;

    Ok(format!(
        "super source: 31 rows, 3 synthetic expansions, {nextest_rows} nextest rows, {refused} malformed tables refused; stress verdict bracketed; {standin}"
    ))
}

/// Run a probe node's generated command against a stand-in Hermit, so the
/// shell itself is exercised and not only inspected: each repetition's row and
/// result line with its cause, the node's exit status, the per-repetition log,
/// the width bound, and a malformed width. A `timeout` shim first on PATH runs
/// the real GNU timeout with a 1 s bound and 1 s kill grace, so the causes are
/// matched against timeout's own messages.
fn stress_standin_bracket(root: &Path) -> Result<String, String> {
    let dir = std::env::temp_dir().join(format!(
        "validate-super-standin-self-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create super stand-in fixture: {error}"))?;
    let result = run_stress_standin(root, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn write_executable(path: &Path, script: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, script)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("cannot make {} executable: {error}", path.display()))
}

fn run_stress_standin(root: &Path, dir: &Path) -> Result<String, String> {
    const REPS: i64 = 9;
    const WIDTH: usize = 3;
    let probe = StressProbe::PtraceStrictVerify;
    let slug = probe.slug();
    // The probe's last argument is hermit-super-<repetition>. Each repetition
    // records how many repetitions were running when it started, so the width
    // bound is measured rather than assumed. Repetition 3 ends its output
    // without a newline; 5 outlives the bound; 6 also ignores SIGTERM; 7 prints
    // timeout's KILL message itself; 8 and 9 exit 137 and 124 themselves.
    // Repetition 6 ignores SIGTERM on its first line, before the bookkeeping:
    // installed after it, a loaded host could let timeout's TERM arrive first,
    // and the repetition exited 124 instead of waiting for the KILL
    // (https://github.com/rrnewton/hermit/issues/4226).
    let standin = dir.join("hermit");
    write_executable(
        &standin,
        &format!(
            "#!/usr/bin/env bash\n\
             rep=${{!#}}; rep=${{rep##*-}}; d={dir}\n\
             [ \"$rep\" = 6 ] && trap '' TERM\n\
             mkdir \"$d/running.$rep\"\n\
             ls -d \"$d\"/running.* | wc -l >\"$d/peak.$rep\"\n\
             sleep 0.2\n\
             rmdir \"$d/running.$rep\"\n\
             case $rep in\n\
             2) exit 7;;\n\
             3) printf 'hermit-super-3'; exit 0;;\n\
             4) echo \"stand-in diagnostic $rep\" >&2; exit 3;;\n\
             5) sleep 5;;\n\
             6) sleep 10;;\n\
             7) echo \"timeout: sending signal KILL to command 'x'\"; exit 5;;\n\
             8) exit 137;;\n\
             9) exit 124;;\n\
             esac\n\
             echo \"hermit-super-$rep\"\n",
            dir = shell_quote(&dir.to_string_lossy())
        ),
    )?;
    // The node calls `timeout --verbose --kill-after=G T <command...>`; the
    // shim drops its own directory from PATH and keeps only the command. Only
    // repetitions 5 and 6 are meant to outlive the bound, so only they get a
    // short one; the rest get 30 s, so a loaded host cannot time out a
    // repetition that should exit on its own (9's own 124 then read as
    // timeout's, at load average 300 on 2026-10-07). Repetition 6 gets 3 s, so
    // its bash has that long to reach its first line, where it ignores TERM,
    // and it sleeps 10 s, longer than the bound and the 1 s grace together.
    let shim_dir = dir.join("bin");
    std::fs::create_dir_all(&shim_dir)
        .map_err(|error| format!("cannot create super stand-in shim dir: {error}"))?;
    write_executable(
        &shim_dir.join("timeout"),
        "#!/usr/bin/env bash\n\
         rep=${!#}; rep=${rep##*-}\n\
         case $rep in 5) bound=1;; 6) bound=3;; *) bound=30;; esac\n\
         PATH=${PATH#*:} exec timeout --verbose --kill-after=1 \"$bound\" \"${@:4}\"\n",
    )?;
    let path = format!(
        "{}:{}",
        shim_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let command = probe.node_command(REPS, &standin.to_string_lossy(), "/nonexistent", dir);
    let run = |width: &str, counts: &Path| {
        std::process::Command::new("bash")
            .arg("-c")
            .arg(&command)
            .current_dir(root)
            .env("PATH", &path)
            .env("DAGRUN_TEST_COUNTS_PATH", counts)
            .env(SUPER_PROBE_JOBS_ENV, width)
            .output()
            .map_err(|error| format!("cannot run super stand-in node: {error}"))
    };

    let counts = dir.join("counts.json");
    let output = run(&WIDTH.to_string(), &counts)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() != Some(1) {
        return Err(format!(
            "super stand-in node with failed repetitions exited {:?}, not 1:\n{stdout}{stderr}",
            output.status.code()
        ));
    }
    if !stderr.is_empty() {
        return Err(format!(
            "super stand-in node wrote untagged stderr:\n{stderr}"
        ));
    }
    let killed = format!(
        " (timeout sent SIGKILL {SUPER_PROBE_KILL_GRACE_S} s after the {SUPER_PROBE_TIMEOUT_S} s bound)"
    );
    let timed_out = format!(" (timed out after {SUPER_PROBE_TIMEOUT_S} s)");
    let own_kill = " (SIGKILL not sent by its timeout, e.g. the OOM killer)";
    for (rep, verdict, cause) in [
        (1, "pass, exit 0,", ""),
        (2, "fail, exit 7,", ""),
        (3, "pass, exit 0,", ""),
        (4, "fail, exit 3,", ""),
        (5, "fail, exit 124,", timed_out.as_str()),
        (6, "fail, exit 137,", killed.as_str()),
        (7, "fail, exit 5,", ""),
        (8, "fail, exit 137,", own_kill),
        (9, "fail, exit 124,", ""),
    ] {
        let prefix = format!("super stress {slug} repetition {rep}/{REPS}: {verdict} ");
        let suffix = format!(" CPU s{cause}");
        if !stdout
            .lines()
            .any(|line| line.starts_with(&prefix) && line.ends_with(&suffix))
        {
            return Err(format!(
                "super stand-in output lacks {prefix:?}...{suffix:?}:\n{stdout}"
            ));
        }
    }
    let first_after_three = format!("super stress {slug} repetition 4/{REPS}:");
    for line in [
        "  [1] hermit-super-1\n".to_string(),
        format!("  [3] hermit-super-3\n{first_after_three}"),
        "  [4] stand-in diagnostic 4\n".to_string(),
        "  [5] timeout: sending signal TERM".to_string(),
        "  [6] timeout: sending signal KILL".to_string(),
    ] {
        if !stdout.contains(&line) {
            return Err(format!(
                "super stand-in output lacks the repetition log line {line:?}:\n{stdout}"
            ));
        }
    }
    let text = std::fs::read_to_string(&counts)
        .map_err(|error| format!("super stand-in wrote no structured results: {error}"))?;
    let document: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("super stand-in results are not JSON: {error}"))?;
    let rows = document["results"]
        .as_array()
        .ok_or_else(|| format!("super stand-in results have no rows: {text}"))?;
    let failed = rows
        .iter()
        .filter(|row| row["result"] == "fail")
        .filter_map(|row| row["id"].as_str())
        .collect::<Vec<_>>();
    let expected_failed = [2, 4, 5, 6, 7, 8, 9].map(|rep| format!("{slug}/repetition-{rep:02}"));
    if document["executed_tests"] != REPS
        || rows.len() != REPS as usize
        || rows.iter().filter(|row| row["result"] == "pass").count() != 2
        || failed != expected_failed
    {
        return Err(format!(
            "super stand-in rows are not passes 01 and 03 with the rest failed: {text}"
        ));
    }
    let mut peak = 0;
    for rep in 1..=REPS {
        let observed = std::fs::read_to_string(dir.join(format!("peak.{rep}")))
            .map_err(|error| format!("super stand-in repetition {rep} did not run: {error}"))?;
        let observed = observed
            .trim()
            .parse::<usize>()
            .map_err(|error| format!("super stand-in repetition {rep} peak: {error}"))?;
        peak = peak.max(observed);
    }
    if !(2..=WIDTH).contains(&peak) {
        return Err(format!(
            "super stand-in ran at most {peak} repetitions at once with width {WIDTH}"
        ));
    }

    let refused_counts = dir.join("refused.json");
    let refused = run("three", &refused_counts)?;
    if refused.status.code() != Some(2) || refused_counts.exists() {
        return Err(format!(
            "super stand-in node accepted a malformed width: exit {:?}",
            refused.status.code()
        ));
    }
    Ok(format!(
        "stand-in probe node: 2/{REPS} passed, the planted failures carried their causes \
         (real timeout TERM and KILL, own 137, own 124 and a printed timeout message), \
         at most {peak} of width {WIDTH} at once, malformed width refused"
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
