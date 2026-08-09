// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Plan construction for the validate driver: turn a PROFILE into a `DagConfig`.
//!
//! # The single rule this module exists to enforce
//!
//! **Nothing validate runs may execute outside `safe-ci-dag-runner`.** Every gate
//! — preflight submodule init, the Reverie pin check, the manifest gate, each CI
//! lane node, and each compatibility probe — is a DAG *node*. The driver makes
//! exactly one kind of call (`run_dag_boxed_ordered`) and never spawns work
//! itself. The previous Phase-1 wrapper had a `run_subprocess_gate` helper that
//! shelled out for the three preflight gates; that was a second execution path
//! inside the driver, so those gates were unboxed, untimed by the runner, and
//! invisible to its typed accounting. It is gone.
//!
//! # Every synthesized node MUST declare its caps — measured, not assumed
//!
//! `safe-ci-dag-runner` applies its SMALL "forcing function" floor (1 GiB / 1 core
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
//! So: every node built here declares `timeout`, `cpu_timeout`, and a memory
//! hint, and [`undeclared_nodes`] is the fail-closed audit that keeps it true.
//!
//! Note also that `ci/dag/{portable,privileged}.json` declare memory hints on
//! 47/47 and 8/8 nodes respectively, but `cpu_timeout` on **0/55** — so the
//! per-step CPU-time guard is currently inert for every shipped lane node. This
//! module supplies a profile-level `default_step_cpu_timeout` so those nodes get
//! a load-immune budget without editing 55 JSON rows.

use std::collections::BTreeMap;
use std::path::Path;

use safe_ci_dag_runner::io::dag_from_json;
use safe_ci_dag_runner::model::DagConfig;
use safe_ci_dag_runner::model::ResourceHint;
use safe_ci_dag_runner::model::Step;

use crate::validate_corpus;
use crate::validate_corpus::CorpusPaths;

/// Wall budget for the preflight gates. Submodule init reaches the network
/// through `with-proxy`, so it needs more than a trivial ceiling but must not
/// inherit a lane-sized one.
const PREFLIGHT_TIMEOUT_S: i64 = 900;
/// CPU budget for preflight. These gates are I/O-bound (clone, fetch, a small
/// rustc); a tight CPU ceiling catches a spin without flaking under host load.
const PREFLIGHT_CPU_TIMEOUT_S: i64 = 300;
/// Memory ceiling for a preflight gate. `git submodule update --recursive` on
/// this tree peaks well under a GiB; 2 GiB leaves headroom without being a
/// non-cap.
const PREFLIGHT_MEM_BYTES: i64 = 2 * 1024 * 1024 * 1024;

/// Per-lane-node CPU budget applied as the DAG-level default, closing the
/// measured 0/55 `cpu_timeout` gap. Generous relative to the wall timeout because
/// the build spine legitimately burns many CPU-minutes; it exists to stop an
/// unbounded spin, not to police normal cost.
const LANE_DEFAULT_CPU_TIMEOUT_S: i64 = 7200;

/// Wall budget for one compatibility probe. Mirrors `STRICT_COMPAT_TIMEOUT=60`
/// (validate.sh:1091).
const COMPAT_TIMEOUT_S: i64 = 60;
/// Shortened budget for a bounded portable diagnostic row (validate.sh:2969).
const COMPAT_PORTABLE_DIAGNOSTIC_TIMEOUT_S: i64 = 20;
/// Extended budget for the two large internal executables under e9patch
/// (validate.sh:2991).
const COMPAT_E9PATCH_LARGE_TIMEOUT_S: i64 = 180;
/// CPU budget for a compatibility probe: these are short guest runs under Hermit,
/// so a spin is the failure mode a CPU cap catches.
const COMPAT_CPU_TIMEOUT_S: i64 = 120;
/// Memory ceiling for a compatibility probe.
const COMPAT_MEM_BYTES: i64 = 4 * 1024 * 1024 * 1024;

/// Which compatibility corpus a focused mode runs, and how it is labelled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompatMode {
    Strict,
    PortableStrict,
    Sabre,
    E9patch,
    Rr,
}

impl CompatMode {
    /// The `ci/compat/corpus-<mode>.json` file this mode reads. `PortableStrict`
    /// shares `strict`'s corpus: `PORTABLE_STRICT_PROBE_ARGS` changes the Hermit
    /// FLAGS, never corpus membership (validate.sh:2965).
    pub fn corpus_name(self) -> &'static str {
        match self {
            CompatMode::Strict | CompatMode::PortableStrict => "strict",
            CompatMode::Sabre => "sabre",
            CompatMode::E9patch => "e9patch",
            CompatMode::Rr => "rr",
        }
    }

    /// The assurance label printed per row, mirroring `assurance` in
    /// `strict_compatibility_probe`.
    pub fn assurance(self) -> &'static str {
        match self {
            CompatMode::Strict | CompatMode::PortableStrict => "L2",
            CompatMode::Sabre => "SaBRe",
            CompatMode::E9patch => "e9patch L2",
            CompatMode::Rr => "rr",
        }
    }

    /// The `hermit run ...` flags preceding `--`, reproducing the `run_args`
    /// selection in `strict_compatibility_probe` (validate.sh:2964-2994).
    pub fn run_args(self, label: &str, nsswitch: &str) -> Vec<String> {
        let s = |v: &str| v.to_string();
        match self {
            CompatMode::Strict => vec![s("run"), s("--strict"), s("--verify"), s("--")],
            CompatMode::PortableStrict => vec![
                s("run"),
                s("--strict"),
                s("--verify"),
                s("--no-virtualize-cpuid"),
                s("--max-timeslice=disabled"),
                s("--"),
            ],
            CompatMode::Sabre => {
                vec![s("run"), s("--backend"), s("sabre"), s("--strict"), s("--verify"), s("--")]
            }
            CompatMode::E9patch => {
                let mut v = vec![s("run"), s("--backend"), s("e9patch")];
                // These rows query owner names the host may delegate to an async
                // identity daemon; pin just them to the files-only NSS fixture
                // (validate.sh:2981).
                if matches!(label, "whoami" | "groups" | "pinky" | "logname" | "tar" | "chown") {
                    v.push(format!(
                        "--mount=type=bind,source={nsswitch},target=/etc/nsswitch.conf,readonly"
                    ));
                }
                v.push(s("--strict"));
                v.push(s("--verify"));
                v.push(s("--"));
                v
            }
            // rr rows are driven through `hermit record start --verify`, matching
            // rr_compatibility_probe rather than the plain run path.
            CompatMode::Rr => {
                vec![s("record"), s("start"), s("--verify"), s("--verify-strict"), s("--")]
            }
        }
    }

    /// Per-row wall budget, reproducing the two budget overrides the bash applies.
    pub fn timeout_for(self, label: &str) -> i64 {
        if self == CompatMode::PortableStrict
            && validate_corpus::portable_diagnostic().contains_key(label)
        {
            return COMPAT_PORTABLE_DIAGNOSTIC_TIMEOUT_S;
        }
        if self == CompatMode::E9patch && matches!(label, "mysql" | "php") {
            return COMPAT_E9PATCH_LARGE_TIMEOUT_S;
        }
        COMPAT_TIMEOUT_S
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
        deps,
        env: BTreeMap::new(),
        hint: ResourceHint {
            rss_baseline_bytes: Some(mem_bytes),
            hard_mem_max_bytes: Some(mem_bytes),
            ..Default::default()
        },
        networkonly: false,
        engine_only: false,
        timeout,
        cpu_timeout,
        jobs_flag: None,
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

/// The three always-on preflight gates, as DAG nodes.
///
/// `validate.sh` runs these before every profile and fails fast if either of the
/// first two fails (validate.sh:4745-4752); the dependency edges below reproduce
/// that fail-fast structurally — a failed dependency SKIPS its dependents, which
/// the runner reports as `skipped` rather than as passes.
pub fn preflight_nodes(root: &Path, with_proxy: bool) -> Vec<Step> {
    let proxy = if with_proxy { "with-proxy " } else { "" };
    // The Reverie-pin launcher is bound to THIS repository explicitly, never left
    // to whatever directory the node happens to start in. `ci/test_harness.sh`'s
    // `assert_reverie_pin_enforcement` audits that binding, because "it will be
    // the right repo because cwd is right" is an inference, not an observation —
    // and the archival pin is not a testing exemption.
    let root = shell_quote(&root.to_string_lossy());
    vec![
        node(
            "pre",
            "submodules",
            "Initialize repository submodules",
            format!(
                "{proxy}git submodule update --init --recursive && \
                 status=$(git submodule status --recursive) && printf '%s\\n' \"$status\" && \
                 ! printf '%s\\n' \"$status\" | grep -Eq '^[-+U]' && \
                 test -f agent-utils/README.md && test -f third-party/rr/CMakeLists.txt"
            ),
            vec![],
            PREFLIGHT_TIMEOUT_S,
            PREFLIGHT_CPU_TIMEOUT_S,
            PREFLIGHT_MEM_BYTES,
        ),
        node(
            // Tag must stay `pre.reverie_pin`: scripts/validate.rs asserts a
            // passing node with exactly this tag before it will emit a PASS.
            "pre",
            "reverie_pin",
            "Reverie pin consistency",
            format!("{proxy}{root}/ci/run-reverie-pin-check.sh --repo {root}"),
            vec!["pre.submodules".to_string()],
            PREFLIGHT_TIMEOUT_S,
            PREFLIGHT_CPU_TIMEOUT_S,
            PREFLIGHT_MEM_BYTES,
        ),
        node(
            "gate",
            "manifest",
            "Centralized test manifest and inventory",
            "./ci/test_harness.sh validate".to_string(),
            vec!["pre.reverie_pin".to_string()],
            PREFLIGHT_TIMEOUT_S,
            PREFLIGHT_CPU_TIMEOUT_S,
            PREFLIGHT_MEM_BYTES,
        ),
    ]
}

/// THE one place a CI lane's file is resolved.
///
/// `ci/test_harness.sh` audits that this expression appears EXACTLY ONCE in this
/// file, so that a lane's node set can never be resolved from two places that
/// could drift. Both `lane_nodes` (steps) and `lane_config` (top-level config)
/// go through here; adding a second construction of the path is what the audit
/// exists to catch, and it caught exactly that when `lane_config` was added.
pub fn lane_dag_path(root: &Path, lane: &str) -> std::path::PathBuf {
    root.join("ci").join("dag").join(format!("{lane}.json"))
}

/// Load one shipped CI lane (`ci/dag/<lane>.json`) and hang it off the preflight.
///
/// `prefix` disambiguates tags when two lanes are fused into one DAG; it is empty
/// for a single-lane run so tags stay byte-identical to the shipped file (which
/// keeps `ci/run-node.sh`, the perf store, and the coverage predicate keyed the
/// same way).
pub fn lane_nodes(
    root: &Path,
    lane: &str,
    prefix: &str,
    gate_dep: &str,
) -> Result<Vec<Step>, String> {
    let path = lane_dag_path(root, lane);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let cfg = dag_from_json(&text).map_err(|e| format!("invalid DAG {}: {e}", path.display()))?;
    let retag = |g: &str| if prefix.is_empty() { g.to_string() } else { format!("{prefix}{g}") };
    let mut out = Vec::with_capacity(cfg.steps.len());
    for s in &cfg.steps {
        let mut step = s.clone();
        step.group = retag(&s.group);
        step.deps = s
            .deps
            .iter()
            .map(|d| match d.split_once('.') {
                Some((g, j)) => format!("{}.{}", retag(g), j),
                None => d.clone(),
            })
            .collect();
        // Every lane node waits on the manifest gate, reproducing
        // run_ci_manifest_lane's ordering (validate.sh:4344).
        if step.deps.is_empty() {
            step.deps.push(gate_dep.to_string());
        }
        // Supply a memory cap for any lane node that shipped without one, so the
        // "declared caps" audit below cannot be satisfied by an unboxed node.
        if step.hint.rss_baseline_bytes.is_none() && step.hint.hard_mem_max_bytes.is_none() {
            step.hint.hard_mem_max_bytes = Some(8 * 1024 * 1024 * 1024);
        }
        out.push(step);
    }
    Ok(out)
}

/// Build the compatibility-corpus nodes for one mode.
///
/// One DAG node PER PROBE. That is a deliberate change from the bash, which ran
/// all ~191 probes serially inside a single gate:
///   * each probe now gets its own wall + CPU + memory box, so one runaway row
///     cannot consume the whole gate's budget;
///   * each probe's verdict is a TYPED `StepOutcome`, so the summary table is
///     built from structured results instead of a scraped TSV; and
///   * the corpus becomes parallel, which is where a large part of the wall-clock
///     win in this profile is expected to come from.
pub fn compat_nodes(
    root: &Path,
    mode: CompatMode,
    hermit_bin: &str,
    nsswitch: &str,
    paths: &CorpusPaths,
    gate_dep: Option<&str>,
) -> Result<Vec<Step>, String> {
    compat_nodes_for(root, mode, hermit_bin, nsswitch, paths, gate_dep, None, None)
}

/// [`compat_nodes`] with two extra knobs used by the `super` suite's
/// `run_portable_slow_strict_diagnostics` port (validate.sh:4603).
///
/// * `only` restricts the corpus to an explicit label set AND suppresses the
///   `PORTABLE_STRICT_SUPER_ONLY` skip — because that gate exists precisely to
///   defer those four heavy rows *to this suite*, so the suite that runs them
///   must not also honor the deferral.
/// * `wall_override` replaces the per-row 60s corpus budget. The bash gave the
///   whole group of four one 600s `run_check_with_timeout`; each of these rows
///   is a full compile-link-run or JVM startup workload, so inheriting the
///   group's budget per node is the faithful reading. The 60s corpus default
///   would fail all four for lack of time and report it as a compatibility loss.
#[allow(clippy::too_many_arguments)]
pub fn compat_nodes_for(
    root: &Path,
    mode: CompatMode,
    hermit_bin: &str,
    nsswitch: &str,
    paths: &CorpusPaths,
    gate_dep: Option<&str>,
    only: Option<&std::collections::BTreeSet<String>>,
    wall_override: Option<i64>,
) -> Result<Vec<Step>, String> {
    let rows = validate_corpus::load(root, mode.corpus_name(), paths)?;
    let rr_allowed: Vec<&str> = validate_corpus::RR_PASSING_LABELS.to_vec();
    let super_only = validate_corpus::portable_super_only();
    let mut out = Vec::new();
    for row in rows {
        if let Some(keep) = only {
            if !keep.contains(&row.label) {
                continue;
            }
        }
        // rr measures ONLY the labels proven to pass record/replay; the bash
        // applies the same filter inside rr_compatibility_probe.
        if mode == CompatMode::Rr && !rr_allowed.contains(&row.label.as_str()) {
            continue;
        }
        // Heavy runtime workloads are deferred out of the portable profile to the
        // scheduled super suite (validate.sh:3090) — unless this IS that suite,
        // which names them explicitly through `only`.
        if only.is_none()
            && mode == CompatMode::PortableStrict
            && super_only.contains_key(row.label.as_str())
        {
            continue;
        }
        let mut argv: Vec<String> = vec![hermit_bin.to_string()];
        argv.extend(mode.run_args(&row.label, nsswitch));
        argv.extend(row.argv.iter().cloned());
        let wall = wall_override.unwrap_or_else(|| mode.timeout_for(&row.label));
        out.push(node(
            "compat",
            &sanitize_job(&row.label),
            &format!("{} compatibility: {}", mode.assurance(), row.label),
            format!("{} </dev/null", shell_join(&argv)),
            gate_dep.map(|d| vec![d.to_string()]).unwrap_or_default(),
            wall,
            COMPAT_CPU_TIMEOUT_S.max(wall),
            COMPAT_MEM_BYTES,
        ));
    }
    if out.is_empty() {
        return Err(format!("compatibility mode {mode:?} selected zero probes"));
    }
    Ok(out)
}

/// Keep only the named lane nodes, pruning each survivor's deps to the kept set.
///
/// Port of `build_selected_portable_dag` (validate.sh:4400), which did the same
/// `jq` surgery into a temporary DAG file consumed through
/// `RUN_DAG_FILE_OVERRIDE`. Here the plan is already in memory, so no temp file
/// and no second DAG-loading path are involved.
///
/// `ci/select-tests.rs` emits a dependency-CLOSED node set, so pruning cannot
/// drop a genuine dependency — but that is the selector's guarantee, not this
/// function's, so the caller is told how many edges were pruned and how many of
/// the requested tags were not found. An unknown tag is a selector/DAG mismatch
/// and is reported rather than silently ignored.
pub struct Selection {
    pub steps: Vec<Step>,
    pub pruned_edges: usize,
    pub unknown_tags: Vec<String>,
}

pub fn select_lane_nodes(all: Vec<Step>, keep: &std::collections::BTreeSet<String>) -> Selection {
    let present: std::collections::BTreeSet<String> = all.iter().map(|s| s.tag()).collect();
    let unknown_tags: Vec<String> = keep.difference(&present).cloned().collect();
    let mut pruned_edges = 0usize;
    let steps = all
        .into_iter()
        .filter(|s| keep.contains(&s.tag()))
        .map(|mut s| {
            let before = s.deps.len();
            s.deps.retain(|d| keep.contains(d) || !present.contains(d));
            pruned_edges += before - s.deps.len();
            s
        })
        .collect();
    Selection { steps, pruned_edges, unknown_tags }
}

/// DAG tags are `group.job`, so a job containing `.` would produce an ambiguous
/// tag. Corpus labels are shell-command names (`c++filt`, `wc-lines`), none of
/// which contain a dot today, but the mapping is applied rather than assumed.
pub fn sanitize_job(label: &str) -> String {
    label.replace('.', "_")
}

/// Assemble a `DagConfig` from steps, applying the profile-level CPU-time default
/// that the shipped lane JSON does not carry.
/// Load a lane's FULL `DagConfig` -- not just its steps.
///
/// `lane_nodes` returns steps because the fusion path rewrites their tags, but a
/// DAG file is more than a bag of steps: `resource_caps`, `default_step_timeout`,
/// `mem_cap_factor`, `mem_cap_floor_bytes` and `outer_mem_safety_factor` are all
/// top-level, and every one of them silently reverts to `DagConfig::default()` if
/// the caller rebuilds the config instead of carrying it.
pub fn lane_config(root: &Path, lane: &str) -> Result<DagConfig, String> {
    let path = lane_dag_path(root, lane);
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
/// `ci/dag/portable.json` declares `resource_caps {hermit_guest: 1,
/// manifest_guest: 4}`; dropping them left `res_free` evaluating
/// `unwrap_or(0) >= 1` for the 16 steps demanding `hermit_guest` and the 13
/// demanding `manifest_guest`, so none could ever be admitted. The scheduler's
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
    // The one DELIBERATE divergence: shipped lane nodes declare cpu_timeout on
    // 0 of 55, so supply a load-immune default. A node's own cpu_timeout still
    // wins via effective_cpu_timeout. Recorded here so the audit can exempt it.
    cfg.default_step_cpu_timeout = LANE_DEFAULT_CPU_TIMEOUT_S;
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
        bad.push(format!("resource_caps {:?} != {:?}", base.resource_caps, derived.resource_caps));
    }
    if base.mem_cap_factor != derived.mem_cap_factor {
        bad.push(format!("mem_cap_factor {} != {}", base.mem_cap_factor, derived.mem_cap_factor));
    }
    if base.mem_cap_floor_bytes != derived.mem_cap_floor_bytes {
        bad.push(format!("mem_cap_floor_bytes {} != {}", base.mem_cap_floor_bytes, derived.mem_cap_floor_bytes));
    }
    if base.outer_mem_safety_factor != derived.outer_mem_safety_factor {
        bad.push(format!("outer_mem_safety_factor {} != {}", base.outer_mem_safety_factor, derived.outer_mem_safety_factor));
    }
    if base.default_step_timeout != derived.default_step_timeout {
        bad.push(format!("default_step_timeout {} != {}", base.default_step_timeout, derived.default_step_timeout));
    }
    if base.default_jobs_flag != derived.default_jobs_flag {
        bad.push(format!("default_jobs_flag {:?} != {:?}", base.default_jobs_flag, derived.default_jobs_flag));
    }
    if base.default_step_mem_cap_bytes != derived.default_step_mem_cap_bytes {
        bad.push(format!("default_step_mem_cap_bytes {:?} != {:?}", base.default_step_mem_cap_bytes, derived.default_step_mem_cap_bytes));
    }
    if base.default_step_cpu_count != derived.default_step_cpu_count {
        bad.push(format!("default_step_cpu_count {:?} != {:?}", base.default_step_cpu_count, derived.default_step_cpu_count));
    }
    if bad.is_empty() { Ok(()) } else { Err(bad.join("; ")) }
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
                bad.push(format!("{} demands {r}={n} but resource_caps grants {cap}", s.tag()));
            }
        }
    }
    bad
}

/// Fail-closed audit: every node in a plan must declare a wall timeout, a CPU
/// budget (its own or the config default), and a memory cap.
///
/// This is the guard that keeps the module doc's claim true as nodes are added.
/// Without it, a future node added without hints would run UNBOXED while the
/// driver still printed "cgroup boxing ACTIVE" — a green that verified less than
/// it claimed, which is precisely the failure class this port exists to remove.
///
/// Returns the tags of any nodes that are not fully declared.
pub fn undeclared_nodes(cfg: &DagConfig) -> Vec<String> {
    cfg.steps
        .iter()
        .filter(|s| {
            let mem = s.hint.hard_mem_max_bytes.is_some() || s.hint.rss_baseline_bytes.is_some();
            let cpu = s.cpu_timeout > 0 || cfg.default_step_cpu_timeout > 0;
            let wall = s.timeout > 0;
            !(mem && cpu && wall)
        })
        .map(|s| s.tag())
        .collect()
}
