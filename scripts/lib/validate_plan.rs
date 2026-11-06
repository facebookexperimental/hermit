// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Committed-plan selection and audit helpers for the validate driver.
//!
//! # The single rule this module exists to enforce
//!
//! **Nothing validate runs may execute outside `dagrun`.** Every gate --
//! preflight submodule init, the Reverie pin check, the manifest gate, each CI
//! node, and each compatibility probe -- is a node in the single committed
//! `ci/dag/validate.json`. Profiles select that immutable graph by labels; this
//! module does not compile or rewrite a runtime graph. The driver makes exactly
//! one kind of call (`run_dag_boxed_ordered`) and never spawns work itself. The
//! previous Phase-1 wrapper had a `run_subprocess_gate` helper that
//! shelled out for the three preflight gates; that was a second execution path
//! inside the driver, so those gates were unboxed, untimed by the runner, and
//! invisible to its typed accounting. It is gone.
//!
//! # Every synthesized node MUST declare its caps — measured, not assumed
//!
//! `dagrun` applies its SMALL "forcing function" floor (1 GiB / 1 core
//! / 10 s CPU) **only** through its own CLI, behind `--small-default-cap`. A
//! LIBRARY consumer — which this driver is — gets `DagConfig::default()`, i.e.
//! `default_step_mem_cap_bytes: None`, `default_step_cpu_count: None`,
//! `default_step_cpu_timeout: 0`. That is deliberate on the runner's side (an
//! always-on floor would wedge concurrent validates on the shared checkout), but
//! it means **an undeclared node is boxed in name only**.
//!
//! Measured on this box at the time of writing, through this exact library path:
//! a node declaring nothing allocated 2 GiB and burned 40 s of CPU and PASSED.
//! A node declaring `hard_mem_max_bytes = 256 MiB` and allocating 4 GiB was
//! `OOM-KILLED (hit inner MemoryMax; 3 oom_kill event(s))` at `peak≈256.0 MiB`
//! and failed the run. Boxing works; it just has to be asked for.
//!
//! So every committed node declares `timeout`, `cpu_timeout`, and a memory
//! hint, and `generate-validation-dag --check` (`validation_dag::assert_declared_caps`)
//! is the fail-closed audit that keeps it true.
//! The committed graph deliberately has no global CPU fallback: the generator
//! records each measured or inherited node budget explicitly, including the
//! hosted-privileged repair that replaced the retired graph's unusable implicit
//! 10-second forcing default.

use std::collections::BTreeMap;
use std::path::Path;

use dagrun::io::dag_from_json;
use dagrun::model::CmdType;
use dagrun::model::DagConfig;
use dagrun::model::ResourceHint;
use dagrun::model::Step;
pub use hermit_manifest_plan::host_capability::HostCapability;
pub use hermit_manifest_plan::host_capability::cpuid_faulting_absent;
pub use hermit_manifest_plan::host_capability::kvm_absent;
pub use hermit_manifest_plan::host_capability::probe_host_capability;

/// The canonical test epoch the strict compatibility probes pass, from its one
/// source: `hermit run --strict` refuses an epoch drawn from the host clock.
#[allow(dead_code)]
#[path = "../../detcore-model/src/canonical_test_epoch.rs"]
pub mod canonical_test_epoch;

/// The manifest audit is an executable consumer, so its producer is part of
/// the always-on preflight spine rather than an incidental lane root.
pub const MANIFEST_PLAN_PRODUCER_TAG: &str = "setup.manifest_plan";
pub const MANIFEST_PLAN_BUILD_COMMAND: &str = "cargo build -p hermit-manifest-plan --bins";
pub const MANIFEST_AUDIT_COMMAND: &str = "target/debug/test-harness validate";

/// CPU fallback for synthetic configs used by generator and self-test fixtures.
/// Production profile execution selects nodes whose CPU budgets are explicit in
/// the committed DAG; this value is never a runtime profile rewrite.
const SYNTHETIC_DEFAULT_CPU_TIMEOUT_S: i64 = 7200;

/// Which compatibility corpus a focused mode runs, and how it is labelled.
///
/// One mode remains: the e9patch-compat-only profile's per-program probes from
/// `ci/compat/corpus-e9patch.json`. The portable strict corpus that was the
/// other mode is compat.yaml's (<https://github.com/rrnewton/hermit/issues/4276>).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompatMode {
    E9patch,
}

impl CompatMode {
    /// Plain-language name printed per row and in the summary.
    pub fn display_name(self) -> &'static str {
        match self {
            CompatMode::E9patch => "e9patch legacy below-L2 stripped verify",
        }
    }

    /// The `hermit ...` arguments preceding `--`, reproducing the `run_args`
    /// selection in `strict_compatibility_probe` (validate.sh:2964-2994).
    pub fn run_args(self, label: &str, nsswitch: &str, proc: Option<&str>) -> Vec<String> {
        let s = |v: &str| v.to_string();
        match self {
            CompatMode::E9patch => {
                let mut v = vec![s("--backend"), s("e9patch"), s("run")];
                // These rows query owner names the host may delegate to an async
                // identity daemon; pin just them to the files-only NSS fixture
                // (validate.sh:2981).
                if matches!(
                    label,
                    "whoami" | "groups" | "pinky" | "logname" | "tar" | "chown"
                ) {
                    v.push(format!(
                        "--mount=type=bind,source={nsswitch},target=/etc/nsswitch.conf,readonly"
                    ));
                }
                // A row that reads a /proc name the minimal level omits
                // (vmstat, lsirq) declares its level in the corpus, as
                // compat.yaml rows do; without it the guest fails with
                // "/proc must be mounted" before any comparison.
                if let Some(level) = proc {
                    v.push(format!("--proc={level}"));
                }
                v.push(s("--strict"));
                v.push(s(canonical_test_epoch::CANONICAL_TEST_EPOCH_ARG));
                v.push(s("--verify"));
                v.push(s("--"));
                v
            }
        }
    }
}

/// Build a fully-declared node. This is the ONLY node constructor the plan
/// modules use, so a node cannot be created without caps. It is `pub(crate)` in
/// spirit — `validate_super` and `validate_envelope` call it precisely so that
/// their nodes cannot skip the cap declaration either.
pub fn node(
    group: &str,
    job: &str,
    desc: &str,
    cmd: String,
    deps: Vec<String>,
    timeout: i64,
    cpu_timeout: i64,
    mem_bytes: i64,
) -> Step {
    Step {
        group: group.to_string(),
        job: job.to_string(),
        desc: desc.to_string(),
        description: String::new(),
        cmd,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        result_manifests: None,
        labels: Vec::new(),
        deps,
        env: BTreeMap::new(),
        hint: ResourceHint {
            rss_baseline_bytes: Some(mem_bytes),
            hard_mem_max_bytes: Some(mem_bytes),
            ..Default::default()
        },
        networkonly: false,
        engine_only: false,
        delegated_children: false,
        timeout,
        cpu_timeout,
        priority: 0,
        jobs_flag: None,
        jobs_env: None,
        skip_reason: None,
        // `None` means "this step declares nothing", which is what every node here
        // meant before the runner grew these fields. `Some(vec![])` would be the
        // stronger claim that the step writes to none of the policy's protected
        // domains, and nothing in this plan has established that. Hermit's DAGs set
        // no write-domain policy, so `require_explicit` is false and an omitted
        // declaration is accepted rather than silently treated as a guarantee.
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
        fail_fast_family: None,
        publishes: Vec::new(),
        artifact_deps: Vec::new(),
    }
}

/// Shell-quote one argv element for embedding in a `bash -c` command string.
///
/// The corpus carries argv ARRAYS (that is how it was extracted, and it is what
/// keeps a workload containing spaces, quotes, or `$` from being re-split). The
/// runner takes a single shell string, so each element is single-quoted here with
/// the standard `'\''` escape. Getting this wrong would silently mutate guest
/// commands, so it is exercised by `--self-test`.
pub fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Join an argv into a shell command string.
pub fn shell_join<I: IntoIterator<Item = S>, S: AsRef<str>>(argv: I) -> String {
    argv.into_iter()
        .map(|a| shell_quote(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The always-on preflight gates and the manifest-audit binary producer, as DAG nodes.
///
/// Submodule verification is deliberately non-mutating and first. Initializing
/// or repairing a checkout before observing it would erase the exact drift this
/// gate exists to detect. A caller with an uninitialized checkout must run
/// `make checkout-all` explicitly, then retry validation.
pub fn preflight_nodes(root: &Path) -> Result<Vec<Step>, String> {
    let committed = validation_config(root)?;
    let tags = [
        "pre.submodules",
        "pre.reverie_pin",
        "build.rust_scripts",
        MANIFEST_PLAN_PRODUCER_TAG,
        "gate.manifest",
    ];
    tags.into_iter()
        .map(|tag| {
            committed
                .steps
                .iter()
                .find(|step| step.tag() == tag)
                .cloned()
                .ok_or_else(|| format!("committed validation DAG lost preflight node {tag}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The checkout this file belongs to. rust-script compiles it under its
    /// absolute path; the hermit-manifest-plan library includes it by a path
    /// relative to the workspace root, and runs its tests in ci/manifest-plan.
    fn checkout_root() -> std::path::PathBuf {
        let source = Path::new(file!());
        if source.is_absolute() {
            return source
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .expect("validate_plan.rs lives under scripts/lib")
                .to_path_buf();
        }
        std::env::current_dir()
            .expect("current directory")
            .ancestors()
            .find(|dir| dir.join(source).is_file())
            .expect("an ancestor of the test's directory holds validate_plan.rs")
            .to_path_buf()
    }

    #[test]
    fn manifest_audit_uses_its_measured_cold_cache_cpu_budget_only() {
        let root = checkout_root();
        let nodes = preflight_nodes(&root).unwrap();
        // Each cap is the committed, measured one; see the node descriptions.
        // setup.manifest_plan: 300 s wall since fe11e3855, 1.5 x the 187 s at
        // which its pinned-root clone was killed against the old 180 s cap
        // (https://github.com/rrnewton/hermit/issues/3381).
        // gate.manifest: 120 CPU-s since d7138e26e moved the five tool
        // self-tests into their own selftest.* nodes; the slimmed gate
        // measured 2.53-2.74 CPU-s at load average 215-233. Its 900 s wall
        // is unchanged. CPU re-derived 2026-11-01: across 296 retained
        // full-run step profiles its worst valid CPU was 577.1 s, so the
        // cap is 900, the next 300 s bucket above 1.5 x that worst.
        let expected = vec![
            ("pre.submodules".to_string(), 900, 300, Some(2_147_483_648)),
            ("pre.reverie_pin".to_string(), 900, 300, Some(2_147_483_648)),
            // 1200 s wall since 2026-10-04 (was 900): 1.5 x the 644.496 s
            // peak producer wall across 22 full validations, rounded up to the
            // next 300 s bucket. Its CPU cap is unchanged; its memory cap went
            // from 6 to 10 GiB on 2026-10-16 with width 16 and the release
            // build beside the test-harness build (a 6.4-6.5 GiB cold peak).
            (
                "build.rust_scripts".to_string(),
                1200,
                7200,
                Some(10_737_418_240),
            ),
            // 6 GiB hard cap since 2026-11-05 (#5165): retained runs sat
            // at the former 4-GiB cap (set 2026-09-30 from a 2.53-GiB cold
            // 8-core peak), and fresh uncapped cold builds peaked at
            // 5.00 GiB, so the baseline is that worst peak and the hard
            // cap is 6 GiB. 600 s wall and 2100 CPU-s since 2026-11-02
            // (ci/manifest-plan/src/step_limits.rs): 679 runs peaked at
            // 625.7 CPU-s and 210.3 s of wall.
            (
                "setup.manifest_plan".to_string(),
                600,
                2100,
                Some(6_442_450_944),
            ),
            ("gate.manifest".to_string(), 900, 900, Some(5_368_709_120)),
        ];
        let check = |candidate: &[Step]| {
            let observed = candidate
                .iter()
                .map(|step| {
                    (
                        step.tag(),
                        step.timeout,
                        step.cpu_timeout,
                        step.hint.hard_mem_max_bytes,
                    )
                })
                .collect::<Vec<_>>();
            (observed == expected).then_some(()).ok_or(observed)
        };

        assert_eq!(check(&nodes), Ok(()));

        // The gate's earlier CPU caps (300, 600, then 900 until d7138e26e,
        // then 120 for the slimmed gate) no longer match the measured cap.
        // Restoring any of them would silently hand the gate that budget again.
        for old_cpu_cap in [120, 300, 600] {
            let mut widened_gate = nodes.clone();
            widened_gate
                .iter_mut()
                .find(|step| step.tag() == "gate.manifest")
                .expect("gate.manifest exists")
                .cpu_timeout = old_cpu_cap;
            assert!(
                check(&widened_gate).is_err(),
                "restoring gate.manifest's old {old_cpu_cap}-second CPU cap must fail"
            );
        }

        let mut lowered_gate = nodes.clone();
        lowered_gate
            .iter_mut()
            .find(|step| step.tag() == "gate.manifest")
            .expect("gate.manifest exists")
            .cpu_timeout = 60;
        assert!(
            check(&lowered_gate).is_err(),
            "lowering gate.manifest's measured 900-second CPU cap must fail"
        );

        let mut old_plan_wall = nodes.clone();
        old_plan_wall
            .iter_mut()
            .find(|step| step.tag() == "setup.manifest_plan")
            .expect("setup.manifest_plan exists")
            .timeout = 180;
        assert!(
            check(&old_plan_wall).is_err(),
            "restoring the 180-second setup.manifest_plan wall cap that killed a 187-second run must fail"
        );

        let mut widened_neighbor = nodes;
        widened_neighbor
            .iter_mut()
            .find(|step| step.tag() == "pre.reverie_pin")
            .expect("pre.reverie_pin exists")
            .cpu_timeout = 600;
        assert!(
            check(&widened_neighbor).is_err(),
            "widening a neighboring lightweight preflight cap must fail"
        );
    }
}

pub fn validation_dag_path(root: &Path) -> std::path::PathBuf {
    root.join("ci").join("dag").join("validate.json")
}

// ------------------------------------------------- host-capability requirements
//
// A node can require a facility the MACHINE either has or does not have. Before
// this section such a node had exactly two outcomes — it passed, or it failed —
// so "this host cannot run it" and "this ran and it is broken" were the same
// record. On a host without CPUID faulting `privileged-cpuid.faulting` failed in
// 0.11 s with exit 101 and an empty detail block, which reads like a broken
// build, and its eager-exit cost the other twelve in-flight nodes
// (hermit#2135, hermit#2148, hermit#2205).
//
// The fix is a THIRD recorded outcome, host-inapplicable, decided BEFORE the
// node is spawned. Five properties keep it from becoming a way to excuse a node
// that is merely broken:
//
//  1. The decision never reads the node. It is made from an out-of-band probe of
//     the machine during plan construction; a node's exit code, stderr, or panic
//     message cannot produce it. A broken node still runs, still fails, and is
//     still refused.
//  2. The capability vocabulary is CLOSED and shared with stress-series
//     ([`HostCapability`]). A DAG naming an unknown capability is a
//     plan-construction refusal, not a skip.
//  3. The probe fails closed TOWARD RUNNING. Absence requires two independent
//     sources to agree; a probe error, an unexpected errno, an unreadable
//     `/proc/cpuinfo`, or disagreement between the sources all resolve to
//     PRESENT, so the node runs and any failure is real.
//  4. There is no override that manufactures absence.
//     `HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT` can only force a capability
//     PRESENT, i.e. can only cause MORE to run.
//  5. Recording it honestly is what costs the receipt. The node is written to
//     the ledger as a typed intentional skip whose reason is `host-inapplicable`,
//     and the parent's separately-reviewed consumer allowlist
//     (`ci-hub/validate/gate_completeness.py::ALLOWED_INTENTIONAL_SKIP_REASONS`,
//     `ci-hub/lib/qualifying_receipt.rs::intentional_skip_count`) admits only
//     `empty-manifest-bucket`. A run carrying a host-inapplicable node therefore
//     does NOT qualify as landing authority until the owner opts that reason in.
//     The mechanism cannot buy a green; it can only stop one node's absence from
//     destroying the other forty.

/// The stable ledger reason for a node the machine provably cannot run.
///
/// The word is the owner's, from hermit#2205: "The missing concept is a third,
/// recorded outcome: **host-inapplicable**, distinct from both pass and fail."
pub const HOST_INAPPLICABLE_REASON: &str = "host-inapplicable";

/// One node the machine provably cannot run, and why.
#[derive(Clone, Debug)]
pub struct HostInapplicableNode {
    pub tag: String,
    pub capability: HostCapability,
    pub evidence: String,
    /// `None`: the whole node is withheld and never spawns. `Some`: the node
    /// RUNS, but `target/debug/test-harness` withholds some of its manifest
    /// cells as host-inapplicable. Either way the run did not cover what those
    /// cells verify, so both count against completeness alike.
    pub cells: Option<WithheldCells>,
}

/// The cells of a RUNNING manifest bucket node that the harness withholds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WithheldCells {
    pub withheld: usize,
    pub selected: usize,
}

impl HostInapplicableNode {
    /// Whether the node itself still runs, with only some cells withheld.
    pub fn runs(&self) -> bool {
        self.cells.is_some()
    }
}

/// The `requires_host_capability` declarations in one lane, keyed by runner tag.
///
/// Reads the sole committed validation DAG through the same path helper.
/// An unparseable capability name is an ERROR: refusing the whole run is the
/// only safe response to a declaration nobody can evaluate.
pub fn lane_host_capability_requirements(
    root: &Path,
    lane: &str,
) -> Result<BTreeMap<String, HostCapability>, String> {
    let cfg = validation_config(root)?;
    let mut out = BTreeMap::new();
    for step in &cfg.steps {
        if !step.labels.iter().any(|label| label == lane) {
            continue;
        }
        for capability in [HostCapability::CpuidFaulting, HostCapability::Kvm] {
            if step.labels.iter().any(|label| label == capability.value()) {
                out.insert(step.tag(), capability);
            }
        }
    }
    Ok(out)
}

/// Every `requires_host_capability` declaration in every shipped lane, under
/// both the bare and the committed prefixed tag spelling, so the caller can look a plan's
/// tags up directly however the plan was assembled.
pub fn host_capability_requirements(
    root: &Path,
) -> Result<BTreeMap<String, HostCapability>, String> {
    let mut out = BTreeMap::new();
    for lane in ["portable", "privileged", "full", "quick", "super"] {
        out.extend(lane_host_capability_requirements(root, lane)?);
    }
    Ok(out)
}

/// Split a step list into what will run and what the machine cannot run.
///
/// PURE: it consumes an already-resolved absence map, so `--self-test` brackets
/// both directions without touching the machine.
///
/// A step is withheld ONLY when it declares a capability that is in `absent`.
/// A step with no declaration is never withheld, whatever is absent — that is
/// what stops this from excusing a node that is merely broken.
///
/// Withholding a node that another RETAINED node depends on would silently
/// orphan work, so it is a refusal rather than a cascade.
pub fn partition_host_inapplicable(
    steps: Vec<Step>,
    requirements: &BTreeMap<String, HostCapability>,
    absent: &BTreeMap<HostCapability, String>,
) -> Result<(Vec<Step>, Vec<HostInapplicableNode>), String> {
    let mut keep = Vec::with_capacity(steps.len());
    let mut withheld = Vec::new();
    for step in steps {
        let tag = step.tag();
        match requirements.get(&tag) {
            Some(capability) if absent.contains_key(capability) => {
                withheld.push(HostInapplicableNode {
                    tag,
                    capability: *capability,
                    evidence: absent[capability].clone(),
                    cells: None,
                })
            }
            _ => keep.push(step),
        }
    }
    let gone: std::collections::BTreeSet<&str> = withheld.iter().map(|n| n.tag.as_str()).collect();
    let mut orphaned = Vec::new();
    for step in &keep {
        for dep in &step.deps {
            if gone.contains(dep.as_str()) {
                orphaned.push(format!("{} depends on {dep}", step.tag()));
            }
        }
    }
    if !orphaned.is_empty() {
        return Err(format!(
            "refusing to withhold a host-inapplicable node that other nodes depend on: {}; \
             a missing capability must not silently cascade into unrun work",
            orphaned.join(", ")
        ));
    }
    Ok((keep, withheld))
}

/// Load the complete committed DAG and select one label with its dependency
/// closure while preserving every top-level scheduler setting.
pub fn lane_config(root: &Path, lane: &str) -> Result<DagConfig, String> {
    let path = validation_dag_path(root);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let cfg = dag_from_json(&text).map_err(|e| format!("invalid DAG {}: {e}", path.display()))?;
    dagrun::select_steps_by_labels(&cfg, &[lane.to_string()])
        .map_err(|e| format!("cannot select label {lane} from {}: {e}", path.display()))
}

/// Load the complete committed validation DAG without selecting a profile.
pub fn validation_config(root: &Path) -> Result<DagConfig, String> {
    let path = validation_dag_path(root);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    dag_from_json(&text).map_err(|e| format!("invalid DAG {}: {e}", path.display()))
}

/// Assemble a `DagConfig`, CARRYING every top-level field from `base`.
///
/// # Why this takes a base at all
///
/// It used to be `DagConfig { steps, ..Default::default() }`, which loaded a DAG
/// file, kept its steps, and threw its configuration away. That is not a
/// hypothetical: it hung a full validate for 14 minutes at 0% CPU.
/// `ci/dag/validate.json` declared `resource_caps {manifest_guest: 8}` (12 since 2026-10-17);
/// dropping it leaves `res_free` evaluating `unwrap_or(0) >= 1` for the 13
/// steps demanding `manifest_guest`, so none can be admitted. The scheduler's
/// only exit is `running.is_empty() && done + skipped >= steps.len()`, so with
/// work neither runnable nor accounted it slept at 50 ms forever -- no error, no
/// exit, 21 of ~58 nodes done.
///
/// `resource_caps` failed LOUDLY (a visible hang). The quieter one matters more:
/// `default_step_timeout` is 600 s in portable and 120 s in privileged, and
/// reverted to `DEFAULT_STEP_TIMEOUT` (1800 s) -- every step's wall cap loosened
/// 3x and 15x respectively, with nothing to see. `mem_cap_factor`,
/// `mem_cap_floor_bytes` and `outer_mem_safety_factor` happen to equal their
/// defaults today, so they would have broken the first time anyone tuned them.
///
/// Hence: carry the base wholesale, and let [`assert_config_carried`] prove it.
pub fn config_from_base(base: &DagConfig, steps: Vec<Step>, description: &str) -> DagConfig {
    let mut cfg = base.clone();
    cfg.steps = steps;
    cfg.description = description.to_string();
    // Synthetic generator/self-test configs use a bounded fallback. Production
    // selections retain the explicit per-node budgets in the committed DAG.
    cfg.default_step_cpu_timeout = SYNTHETIC_DEFAULT_CPU_TIMEOUT_S;
    cfg
}

/// Synthesised plans that have no source DAG file (compat, quick, envelope, ...).
pub fn config_from(steps: Vec<Step>, description: &str) -> DagConfig {
    config_from_base(&DagConfig::default(), steps, description)
}

/// Field-by-field proof that `derived` carried `base`'s configuration.
///
/// Enumerated deliberately rather than derived from a `PartialEq`: a new
/// `DagConfig` field must force a decision here instead of silently defaulting,
/// which is the exact failure this function exists to prevent. `steps` and
/// `description` are expected to differ; `default_step_cpu_timeout` is the one
/// documented divergence above.
pub fn assert_config_carried(base: &DagConfig, derived: &DagConfig) -> Result<(), String> {
    let mut bad: Vec<String> = Vec::new();
    if base.resource_caps != derived.resource_caps {
        bad.push(format!(
            "resource_caps {:?} != {:?}",
            base.resource_caps, derived.resource_caps
        ));
    }
    if base.mem_cap_factor != derived.mem_cap_factor {
        bad.push(format!(
            "mem_cap_factor {} != {}",
            base.mem_cap_factor, derived.mem_cap_factor
        ));
    }
    if base.mem_cap_floor_bytes != derived.mem_cap_floor_bytes {
        bad.push(format!(
            "mem_cap_floor_bytes {} != {}",
            base.mem_cap_floor_bytes, derived.mem_cap_floor_bytes
        ));
    }
    if base.outer_mem_safety_factor != derived.outer_mem_safety_factor {
        bad.push(format!(
            "outer_mem_safety_factor {} != {}",
            base.outer_mem_safety_factor, derived.outer_mem_safety_factor
        ));
    }
    if base.default_step_timeout != derived.default_step_timeout {
        bad.push(format!(
            "default_step_timeout {} != {}",
            base.default_step_timeout, derived.default_step_timeout
        ));
    }
    if base.default_jobs_flag != derived.default_jobs_flag {
        bad.push(format!(
            "default_jobs_flag {:?} != {:?}",
            base.default_jobs_flag, derived.default_jobs_flag
        ));
    }
    if base.default_jobs_env != derived.default_jobs_env {
        bad.push(format!(
            "default_jobs_env {:?} != {:?}",
            base.default_jobs_env, derived.default_jobs_env
        ));
    }
    if base.default_step_mem_cap_bytes != derived.default_step_mem_cap_bytes {
        bad.push(format!(
            "default_step_mem_cap_bytes {:?} != {:?}",
            base.default_step_mem_cap_bytes, derived.default_step_mem_cap_bytes
        ));
    }
    if base.default_step_cpu_count != derived.default_step_cpu_count {
        bad.push(format!(
            "default_step_cpu_count {:?} != {:?}",
            base.default_step_cpu_count, derived.default_step_cpu_count
        ));
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(bad.join("; "))
    }
}

/// FAIL CLOSED on capacity that can never be granted.
///
/// A step demanding a resource the config does not cap is unschedulable FOREVER,
/// and the scheduler expresses that as an infinite 50 ms sleep rather than an
/// error. Refusing up front converts a silent 14-minute hang into a named
/// refusal before a single node runs.
pub fn ungrantable_resources(cfg: &DagConfig) -> Vec<String> {
    let mut bad = Vec::new();
    for s in &cfg.steps {
        for (r, n) in &s.hint.resources {
            let cap = cfg.resource_caps.get(r).copied().unwrap_or(0);
            if cap < *n {
                bad.push(format!(
                    "{} demands {r}={n} but resource_caps grants {cap}",
                    s.tag()
                ));
            }
        }
    }
    bad
}
